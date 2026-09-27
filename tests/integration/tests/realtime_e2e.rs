//! Two-client WebSocket E2E entering `handle_socket` (W-9).
//!
//! All scenarios route through the real `realtime_ws_handler` via TCP.
//! Post-processing helpers skip `Heartbeat` frames and use timeouts so a
//! missing message fails deterministically instead of hanging CI.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    clippy::panic,
    clippy::collapsible_if,
    dead_code
)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orbisync_application::WorldDirectoryStore;
use orbisync_domain::{
    Clock as _, InstanceId, Timestamp, Transform, World, WorldId, WorldInstance,
};
use orbisync_interest::UniformGrid;
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::{DenyAllTicketVerifier, StubTicketVerifier};
use orbisync_server::{
    delivery::DeliveryRegistry,
    realtime_ws::{RealtimeState, realtime_ws_handler},
};
use orbisync_testkit::{FakeWorldDirectoryStore, FixedClock};
use orbisync_world_runtime::RuntimeRegistry;
use prost::Message as ProstMessage;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// constants

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
const RECV_TIMEOUT: Duration = Duration::from_secs(3);
const NOT_RECV_TIMEOUT: Duration = Duration::from_millis(500);
const TICKET: &str = "stub-ticket-123";

// ---------------------------------------------------------------------------
// helpers

include!("common/custom_events.rs");

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

struct TestServer {
    addr: std::net::SocketAddr,
    store: Arc<FakeWorldDirectoryStore>,
    registry: Arc<RuntimeRegistry>,
    state: Arc<RealtimeState>,
    clock: Arc<FixedClock>,
}

async fn spawn_server(
    world: World,
    instance: WorldInstance,
    tickets: Arc<dyn orbisync_realtime::gateway::RealtimeTicketVerifier>,
) -> TestServer {
    let store = Arc::new(FakeWorldDirectoryStore::new());
    let world_id = world.id();
    let instance_id = instance.id();
    store.insert_world(world);
    store.insert_instance(instance);
    // sanity: ensure inserted
    let got = store
        .get_instance(instance_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.world_id(), world_id);

    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(store.clone() as Arc<dyn WorldDirectoryStore>)
        .with_tickets(tickets)
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
    // small yield to let server start
    tokio::time::sleep(Duration::from_millis(50)).await;
    TestServer {
        addr,
        store,
        registry,
        state,
        clock,
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

/// Receive next envelope, skipping Heartbeat frames.
/// Returns `None` on close or decode failure.
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
                // Skip Heartbeat / HeartbeatAck transparently
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

/// Try to receive an envelope within `NOT_RECV_TIMEOUT`; returns true if something arrived.
async fn try_recv_envelope<S>(ws: &mut S) -> bool
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    let fut = async {
        loop {
            let opt = ws.next().await;
            let res = opt?;
            let msg = res.ok()?;
            match msg {
                Message::Binary(data) => {
                    let env = Envelope::decode(data.as_ref()).ok()?;
                    match &env.payload {
                        Some(envelope::Payload::Heartbeat(_))
                        | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                        _ => return Some(env),
                    }
                }
                Message::Close(_) => return None::<Envelope>,
                _ => continue,
            }
        }
    };
    match tokio::time::timeout(NOT_RECV_TIMEOUT, fut).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(_) => false,
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

async fn join_instance<S>(ws: &mut S, instance_id: InstanceId) -> (Envelope, Envelope)
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
    // One is JoinAccepted, one is Snapshot (order: JoinAccepted then Snapshot per realtime_ws.rs)
    let is_accepted = |e: &Envelope| matches!(e.payload, Some(envelope::Payload::JoinAccepted(_)));
    let is_snapshot = |e: &Envelope| matches!(e.payload, Some(envelope::Payload::Snapshot(_)));
    assert!(
        (is_accepted(&first) && is_snapshot(&second))
            || (is_snapshot(&first) && is_accepted(&second)),
        "expected JoinAccepted+Snapshot, got {first:?} and {second:?}"
    );
    (first, second)
}

// ---------------------------------------------------------------------------
// Tests

#[tokio::test]
async fn e2e_handshake_both_clients_receive_server_hello() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;

    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;

    // Clean close
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn e2e_authentication_required_with_deny_all() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let server = spawn_server(world, instance, Arc::new(DenyAllTicketVerifier::new())).await;

    let mut ws = ws_connect(server.addr).await;
    // Still need to perform handshake: gateway will accept hello (non-empty ticket) but W-5 verification rejects after.
    let hello = client_hello_bytes(TICKET);
    ws.send(Message::Binary(hello.into()))
        .await
        .expect("send hello");
    // RV-A C1: verification is now before ServerHello, so DenyAll yields
    // AUTHENTICATION_REQUIRED directly (no ServerHello). Keep tolerant of both
    // old (ServerHello then Error) and new (Error only) order.
    let err = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let msg = ws.next().await.expect("next").expect("ok");
            if let Message::Binary(data) = msg {
                let env = decode_envelope(&data);
                match env.payload {
                    Some(envelope::Payload::Error(e)) if e.code == "AUTHENTICATION_REQUIRED" => {
                        return e;
                    }
                    Some(envelope::Payload::ServerHello(_)) => {
                        // old flow: ignore ServerHello and wait for Error
                        continue;
                    }
                    Some(envelope::Payload::Heartbeat(_))
                    | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                    _ => continue,
                }
            }
        }
    })
    .await
    .expect("timeout waiting for AUTHENTICATION_REQUIRED");

    assert_eq!(err.code, "AUTHENTICATION_REQUIRED");
}

#[tokio::test]
async fn e2e_not_found_instance_does_not_create_actor() {
    let world = make_world();
    let instance = make_instance(world.id(), 10);
    let real_instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    // Record registry size before
    let before = server.registry.inner().lock().expect("lock").len();

    let mut ws = ws_connect(server.addr).await;
    handshake(&mut ws, TICKET).await;

    let unknown = InstanceId::generate();
    assert_ne!(unknown, real_instance_id);
    let join = join_instance_bytes(unknown);
    ws.send(Message::Binary(join.into()))
        .await
        .expect("send join");

    let env = recv_envelope(&mut ws).await.expect("error");
    match env.payload {
        Some(envelope::Payload::Error(e)) => assert_eq!(e.code, "NOT_FOUND"),
        other => panic!("expected NOT_FOUND, got {other:?}"),
    }

    // Give server a moment to handle - registry must not have grown
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = server.registry.inner().lock().expect("lock").len();
    assert_eq!(
        before, after,
        "N-2 check deleted: unknown instance must not create actor"
    );
    assert_eq!(after, 0, "no actor should exist for unknown id");

    let mut retry =
        Envelope::decode(join_instance_bytes(real_instance_id).as_slice()).expect("decode join");
    retry.sequence = 3;
    ws.send(Message::Binary(retry.encode_to_vec().into()))
        .await
        .expect("retry join");
    let accepted = recv_envelope(&mut ws).await.expect("join after rejection");
    assert!(matches!(
        accepted.payload,
        Some(envelope::Payload::JoinAccepted(_))
    ));
    assert_eq!(
        accepted.sequence, 3,
        "responses must continue the connection sequence"
    );
    let snapshot = recv_envelope(&mut ws).await.expect("snapshot after retry");
    assert!(matches!(
        snapshot.payload,
        Some(envelope::Payload::Snapshot(_))
    ));
    assert_eq!(snapshot.sequence, 4);

    // Also verify the real instance still not created (no join yet) and that actor not leaked via another check
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = ws.close(None).await;
}

#[tokio::test]
async fn e2e_join_both_clients_and_capacity_uses_store() {
    // Capacity 1 instance: second join should get INSTANCE_FULL if store capacity is respected,
    // but would succeed if default 100 were used.
    let world = make_world();
    let instance = make_instance(world.id(), 1);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    let join = join_instance_bytes(instance_id);
    b.send(Message::Binary(join.into()))
        .await
        .expect("send join b");
    let env = recv_envelope(&mut b).await.expect("b response");
    match env.payload {
        Some(envelope::Payload::Error(e)) => assert_eq!(e.code, "INSTANCE_FULL"),
        other => panic!("second client with capacity 1 should get INSTANCE_FULL, got {other:?}"),
    }

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn e2e_two_clients_join_and_receive_snapshot() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    let (first_a, second_a) = join_instance(&mut a, instance_id).await;
    // Ensure at least one Snapshot received
    let has_snapshot = matches!(first_a.payload, Some(envelope::Payload::Snapshot(_)))
        || matches!(second_a.payload, Some(envelope::Payload::Snapshot(_)));
    assert!(has_snapshot);

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    let (first_b, second_b) = join_instance(&mut b, instance_id).await;
    let has_snapshot_b = matches!(first_b.payload, Some(envelope::Payload::Snapshot(_)))
        || matches!(second_b.payload, Some(envelope::Payload::Snapshot(_)));
    assert!(has_snapshot_b);

    // Registry should have one actor now
    let count = server.registry.inner().lock().expect("lock").len();
    assert_eq!(count, 1);

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn e2e_delivery_transform_nearby_is_received() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    // Both at origin. A creates/moves entity near origin.
    let entity_a = orbisync_domain::EntityId::generate();
    let pos_near = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let bytes = transform_input_bytes(entity_a, 1, pos_near, 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform");

    // B should receive StateDelta containing that entity (skip heartbeats)
    let env = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let ev = recv_envelope(&mut b).await.expect("b recv");
            if let Some(envelope::Payload::StateDelta(delta)) = ev.payload {
                return delta;
            }
        }
    })
    .await
    .expect("B should receive StateDelta within timeout");
    assert!(
        env.entities
            .iter()
            .any(|e| e.entity_id == entity_a.to_string()),
        "B must receive A's nearby entity delta"
    );

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn e2e_interest_50m_is_culled() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    let entity_a = orbisync_domain::EntityId::generate();

    // Select the transform fragment; other fields of the previous revision
    // can still be queued separately by latest-wins delivery.
    // Fails if an ErrorMessage is received (transform was rejected).
    async fn expect_delta_revision<S>(
        ws: &mut S,
        entity: orbisync_domain::EntityId,
        ctx: &str,
    ) -> u64
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                let ev = recv_envelope(ws)
                    .await
                    .unwrap_or_else(|| panic!("{ctx}: ws closed"));
                match ev.payload {
                    Some(envelope::Payload::StateDelta(delta)) => {
                        if let Some(es) = delta
                            .entities
                            .iter()
                            .find(|e| e.entity_id == entity.to_string() && e.transform.is_some())
                        {
                            return es.revision;
                        }
                        // StateDelta for other entity – keep waiting (should not happen in this test)
                        continue;
                    }
                    Some(envelope::Payload::Error(err)) => {
                        panic!(
                            "{ctx}: expected StateDelta but got Error {}: {}",
                            err.code, err.message
                        )
                    }
                    _ => continue,
                }
            }
        })
        .await
        .unwrap_or_else(|_| panic!("{ctx}: timeout waiting for StateDelta for {}", entity))
    }

    // Create entity at (1,0,0) – auto-creation, expected 1 (ignored for creation but use 1)
    let pos1 = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity_a, 1, pos1, 3).into(),
    ))
    .await
    .expect("send create");
    server.clock.advance_millis(500);
    // Both A (owner, at same pos) and B (origin, distance 1m <30) must receive
    let rev_a = expect_delta_revision(&mut a, entity_a, "A creation").await;
    let rev_b = expect_delta_revision(&mut b, entity_a, "B near creation").await;
    assert_eq!(rev_a, 1, "creation revision must be 1");
    assert_eq!(rev_b, 1);
    let mut expected = rev_a; // 1 – next move checks against 1

    // 9m hops to 50m+: each <10m so validation passes, and 500ms clock advance keeps speed <50
    let hops: &[(f64, bool)] = &[
        (10.0, true),  // 9m from 1, distance 10 <30 → B receives
        (19.0, true),  // 9m, distance 19 <30 → B receives
        (28.0, true),  // 9m, distance 28 <30 → B receives (last near)
        (37.0, false), // 9m, distance 37 >35 hysteresis → B culled
        (46.0, false), // 9m, distance 46 → B culled
        (55.0, false), // 9m, distance 55 → B culled (reaches 50m+)
    ];

    for (offset, (x, should_b_receive)) in hops.iter().copied().enumerate() {
        let inbound_sequence = 4 + u64::try_from(offset).expect("small hop index");
        server.clock.advance_millis(500);
        let pos = orbisync_domain::Vec3::new(x, 0.0, 0.0).expect("vec");
        a.send(Message::Binary(
            transform_input_bytes(entity_a, expected, pos, inbound_sequence).into(),
        ))
        .await
        .expect("send hop");
        // A is at the entity's own position, so always receives its own delta
        let new_rev_a = expect_delta_revision(&mut a, entity_a, &format!("A hop to {x}")).await;
        // Advance expected to new revision for next hop
        if should_b_receive {
            let new_rev_b = expect_delta_revision(
                &mut b,
                entity_a,
                &format!("B should receive near hop at {x}"),
            )
            .await;
            assert_eq!(new_rev_b, new_rev_a, "B and A revisions must match at {x}");
        } else {
            // B must NOT receive – wait 500ms and ensure timeout
            let got = tokio::time::timeout(NOT_RECV_TIMEOUT, async {
                loop {
                    let ev = recv_envelope(&mut b).await;
                    match ev {
                        Some(e) => match e.payload {
                            Some(envelope::Payload::StateDelta(delta)) => {
                                if delta.entities.iter().any(|en| {
                                    en.entity_id == entity_a.to_string() && en.revision >= new_rev_a
                                }) {
                                    return true;
                                }
                            }
                            Some(envelope::Payload::Error(err)) => {
                                panic!(
                                    "B culled hop at {x}: unexpected Error {}: {}",
                                    err.code, err.message
                                )
                            }
                            _ => continue,
                        },
                        None => return false,
                    }
                }
            })
            .await;
            assert!(
                got.is_err(),
                "B must NOT receive StateDelta for culled hop at {x} (interest filtering)"
            );
        }
        expected = new_rev_a;
    }

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
#[allow(clippy::collapsible_if)]
#[allow(clippy::collapsible_match)]
async fn e2e_hysteresis_31m_still_visible() {
    // N-3 wiring (W-6): entity subscribed at 28 m must still be visible at 31 m
    // due to hysteresis (unsubscribe is 35). Without the per-connection
    // `subscribed` map this delta is culled at 31 and the test goes red.
    // Note: avatar radius is 30 (D-2), so an avatar at 31 would be culled by
    // policy even with hysteresis. To observe hysteresis we pre-create the
    // actor with a large avatar radius (50) so new avatars have policy 50.
    let world = make_world();
    let world_id = world.id();
    let instance = make_instance(world_id, 100);
    let instance_id = instance.id();

    // Pre-create actor with large radius before server start so joins reuse it.
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    {
        let descriptor = orbisync_world_runtime::InstanceRuntimeDescriptor {
            instance_id,
            state: orbisync_world_runtime::RuntimeState::Running,
            revision: orbisync_domain::Revision::INITIAL,
        };
        let mut actor = orbisync_world_runtime::actor::InstanceActor::with_avatar_visibility_radius(
            descriptor, 50.0,
        );
        actor.start();
        registry
            .inner()
            .lock()
            .expect("lock")
            .insert(instance_id, actor);
    }
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(Arc::clone(&store) as Arc<dyn WorldDirectoryStore>)
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
    let server = TestServer {
        addr,
        store,
        registry,
        state,
        clock,
    };

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();

    // Create avatar at 28 m (near) – B must receive and subscribes.
    let pos28 = orbisync_domain::Vec3::new(28.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity, 1, pos28, 3).into(),
    ))
    .await
    .expect("send 28");
    // Advance clock after send so delta_seconds is 0.5 (valid).
    server.clock.advance_millis(500);
    let rev_a = {
        let delta = tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                let ev = recv_envelope(&mut a).await.expect("a recv");
                match ev.payload {
                    Some(envelope::Payload::StateDelta(d)) => {
                        if d.entities.iter().any(|e| e.entity_id == entity.to_string()) {
                            return d;
                        }
                    }
                    Some(envelope::Payload::Error(e)) => {
                        panic!(
                            "a 28m: expected StateDelta but got Error {}: {}",
                            e.code, e.message
                        )
                    }
                    _ => {}
                }
            }
        })
        .await
        .expect("a 28m delta");
        delta
            .entities
            .iter()
            .find(|e| e.entity_id == entity.to_string())
            .unwrap()
            .revision
    };
    let rev_b = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let ev = recv_envelope(&mut b).await.expect("b recv");
            match ev.payload {
                Some(envelope::Payload::StateDelta(d)) => {
                    if d.entities.iter().any(|e| e.entity_id == entity.to_string()) {
                        return d
                            .entities
                            .iter()
                            .find(|e| e.entity_id == entity.to_string())
                            .unwrap()
                            .revision;
                    }
                }
                Some(envelope::Payload::Error(e)) => {
                    panic!(
                        "b 28m: expected StateDelta but got Error {}: {}",
                        e.code, e.message
                    )
                }
                _ => {}
            }
        }
    })
    .await
    .expect("b must receive 28m delta");
    assert_eq!(rev_a, rev_b);

    // Move to 31 m – still within hysteresis (35) and policy 50, so B must still receive.
    // Without hysteresis (always 30) this is culled and the next timeout fails.
    server.clock.advance_millis(500);
    let pos31 = orbisync_domain::Vec3::new(31.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity, rev_a, pos31, 4).into(),
    ))
    .await
    .expect("send 31");
    let rev_a2 = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let ev = recv_envelope(&mut a).await.expect("a recv 31");
            match ev.payload {
                Some(envelope::Payload::StateDelta(d)) => {
                    if d.entities.iter().any(|e| e.entity_id == entity.to_string()) {
                        return d
                            .entities
                            .iter()
                            .find(|e| e.entity_id == entity.to_string())
                            .unwrap()
                            .revision;
                    }
                }
                Some(envelope::Payload::Error(e)) => {
                    panic!(
                        "a 31m: expected StateDelta but got Error {}: {}",
                        e.code, e.message
                    )
                }
                _ => {}
            }
        }
    })
    .await
    .expect("a 31m delta");
    let rev_b2 = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let ev = recv_envelope(&mut b).await.expect("b recv 31");
            match ev.payload {
                Some(envelope::Payload::StateDelta(d)) => {
                    if d.entities.iter().any(|e| e.entity_id == entity.to_string()) {
                        return d
                            .entities
                            .iter()
                            .find(|e| e.entity_id == entity.to_string())
                            .unwrap()
                            .revision;
                    }
                }
                Some(envelope::Payload::Error(e)) => {
                    panic!(
                        "b 31m: expected StateDelta but got Error {}: {}",
                        e.code, e.message
                    )
                }
                _ => {}
            }
        }
    })
    .await
    .expect("B must still receive 31 m delta due to hysteresis");
    assert_eq!(rev_a2, rev_b2);

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

#[tokio::test]
async fn e2e_disconnect_reclaims_capacity() {
    let world = make_world();
    let instance = make_instance(world.id(), 1);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    // A joins (capacity 1)
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    // A disconnects - PresenceGuard should Leave and reclaim capacity
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Give server time to process Leave via Drop
    tokio::time::sleep(Duration::from_millis(300)).await;

    // C should now be able to join (no INSTANCE_FULL)
    let mut c = ws_connect(server.addr).await;
    handshake(&mut c, TICKET).await;
    let (first, second) = join_instance(&mut c, instance_id).await;
    let is_error = matches!(first.payload, Some(envelope::Payload::Error(_)))
        || matches!(second.payload, Some(envelope::Payload::Error(_)));
    assert!(
        !is_error,
        "after A disconnect, C should join without INSTANCE_FULL"
    );
    let has_accepted = matches!(first.payload, Some(envelope::Payload::JoinAccepted(_)))
        || matches!(second.payload, Some(envelope::Payload::JoinAccepted(_)));
    assert!(has_accepted);

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = c.close(None).await;
}

#[tokio::test]
async fn e2e_viewer_seeded_from_default_spawn() {
    // N-4 (W-7): join's viewer_pos is seeded from World::default_spawn.
    // If the seed stayed at the origin (0,0,0), a world spawned at (100,0,5)
    // would return an empty interest-filtered snapshot and culled deltas for
    // entities near the spawn. With the fix, a second client joining at the
    // same spawn sees deltas for nearby entities.
    // Without the fix (Transform::identity dummy) this test goes red.
    let spawn_pos = orbisync_domain::Vec3::new(100.0, 0.0, 5.0).expect("vec");
    let rot = orbisync_domain::Quaternion::new(0.0, 0.0, 0.0, 1.0).expect("quat");
    let scale = orbisync_domain::Vec3::new(1.0, 1.0, 1.0).expect("vec");
    let spawn = Transform::new(spawn_pos, rot, scale).expect("transform");
    let world = World::new(
        WorldId::generate(),
        "spawn-far-world",
        None,
        spawn,
        100,
        now_ts(),
    )
    .expect("valid world");
    let instance = WorldInstance::new(InstanceId::generate(), world.id(), 100, now_ts())
        .expect("valid instance");
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    // A's avatar is near the spawn (100,0,5). B's viewer is also seeded at
    // (100,0,5), so distance is ~1 m <30 and interest + policy allow it.
    // B must receive the delta. With the old origin-fixed viewer (0,0,0),
    // distance is ~100 m and the delta is culled.
    let entity = orbisync_domain::EntityId::generate();
    let near_spawn = orbisync_domain::Vec3::new(101.0, 0.0, 5.0).expect("vec");
    let bytes = transform_input_bytes(entity, 1, near_spawn, 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform");

    let delta = tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let ev = recv_envelope(&mut b).await.expect("b recv");
            if let Some(envelope::Payload::StateDelta(d)) = ev.payload {
                if d.entities.iter().any(|e| e.entity_id == entity.to_string()) {
                    return d;
                }
            }
        }
    })
    .await
    .expect("B must receive StateDelta for entity near spawn (N-4 viewer seeded)");

    assert!(
        delta
            .entities
            .iter()
            .any(|e| e.entity_id == entity.to_string()),
        "B must see entity near spawn"
    );

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

/// M-7: per-recipient sequence. Broadcast encodes with `sequence: 0`; the
/// delivery path in `handle_socket` rewrites `env.sequence = next_sequence`
/// for each recipient. If the rewrite is changed to `0`, this test goes red
/// because both clients would see 0 instead of their per-socket counter.
/// If sequencing were global, the second message would still appear correct
/// for a single client but would diverge between clients after independent
/// receives — this test checks both clients see 4 then 5 for the same two
/// broadcasts.
#[tokio::test]
async fn e2e_per_recipient_sequence_is_independent() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    // Helper that waits for StateDelta for `entity` and returns the full envelope.
    async fn recv_delta_envelope<S>(ws: &mut S, entity: orbisync_domain::EntityId) -> Envelope
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                let ev = recv_envelope(ws).await.expect("ws closed");
                if let Some(envelope::Payload::StateDelta(delta)) = &ev.payload {
                    if delta
                        .entities
                        .iter()
                        .any(|e| e.entity_id == entity.to_string())
                    {
                        return ev;
                    }
                }
                if let Some(envelope::Payload::Error(err)) = &ev.payload {
                    panic!(
                        "expected StateDelta but got Error {}: {}",
                        err.code, err.message
                    );
                }
            }
        })
        .await
        .expect("timeout waiting for StateDelta envelope")
    }

    // First entity near origin - both viewers at (0,0,0) default spawn see it.
    let entity = orbisync_domain::EntityId::generate();
    let pos1 = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity, 1, pos1, 3).into(),
    ))
    .await
    .expect("send transform 1");
    server.clock.advance_millis(500);

    let env_a1 = recv_delta_envelope(&mut a, entity).await;
    let env_b1 = recv_delta_envelope(&mut b, entity).await;
    // Breaking `env.sequence = next_sequence` to `env.sequence = 0` makes these 0.
    assert_ne!(
        env_a1.sequence, 0,
        "M-7: first delta must have per-socket sequence, not 0 (breaking next_sequence -> 0 goes red)"
    );
    assert_ne!(
        env_b1.sequence, 0,
        "M-7: first delta for B must have per-socket sequence, not 0"
    );
    assert_eq!(
        env_a1.sequence, env_b1.sequence,
        "same broadcast must yield same sequence value for both recipients (both start at 4)"
    );
    assert_eq!(
        env_a1.sequence, 4,
        "first delivery after JoinAccepted(2)+Snapshot(3) must be 4"
    );
    let rev1 = match &env_a1.payload {
        Some(envelope::Payload::StateDelta(d)) => {
            d.entities
                .iter()
                .find(|e| e.entity_id == entity.to_string())
                .expect("entity")
                .revision
        }
        _ => panic!("not delta"),
    };

    // Second transform, same entity, move 9m forward (still <10m and <50m/s).
    server.clock.advance_millis(500);
    let pos2 = orbisync_domain::Vec3::new(10.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity, rev1, pos2, 4).into(),
    ))
    .await
    .expect("send transform 2");

    let env_a2 = recv_delta_envelope(&mut a, entity).await;
    let env_b2 = recv_delta_envelope(&mut b, entity).await;
    assert_eq!(
        env_a2.sequence,
        env_a1.sequence + 1,
        "per-socket sequence must increment by 1 (breaking increment goes red)"
    );
    assert_eq!(
        env_b2.sequence,
        env_b1.sequence + 1,
        "B per-socket sequence must also increment by 1"
    );
    assert_eq!(
        env_a2.sequence, env_b2.sequence,
        "second broadcast must again yield same sequence for both recipients"
    );
    assert_eq!(env_a2.sequence, 5);

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

/// M-6: sent_at is fresh per send. `handle_socket` sets
/// `env.sent_at_unix_ms = clock.now()` for each delivery. If this is changed
/// to a fixed handshake-time value, the two deltas would have identical
/// timestamps and this test goes red. Uses `FixedClock::advance_millis`
/// to make the clock advance deterministic.
#[tokio::test]
async fn e2e_sent_at_is_fresh_per_delivery() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, Arc::new(StubTicketVerifier::new())).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut b, instance_id).await;

    async fn recv_delta_envelope<S>(ws: &mut S, entity: orbisync_domain::EntityId) -> Envelope
    where
        S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
    {
        tokio::time::timeout(RECV_TIMEOUT, async {
            loop {
                let ev = recv_envelope(ws).await.expect("ws closed");
                if let Some(envelope::Payload::StateDelta(delta)) = &ev.payload {
                    if delta
                        .entities
                        .iter()
                        .any(|e| e.entity_id == entity.to_string() && e.transform.is_some())
                    {
                        return ev;
                    }
                }
                if let Some(envelope::Payload::Error(err)) = &ev.payload {
                    panic!(
                        "expected StateDelta but got Error {}: {}",
                        err.code, err.message
                    );
                }
            }
        })
        .await
        .expect("timeout waiting for StateDelta envelope")
    }

    let entity = orbisync_domain::EntityId::generate();
    // Record clock before first send
    let t_before = server.clock.now().to_unix_millis().expect("ts");
    let pos1 = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        transform_input_bytes(entity, 1, pos1, 3).into(),
    ))
    .await
    .expect("send transform 1");
    let env_b1 = recv_delta_envelope(&mut b, entity).await;
    let t1 = env_b1.sent_at_unix_ms;
    assert_eq!(
        t1, t_before,
        "first delivery sent_at must reflect clock at send time (not handshake time 1_700_000_000_000 fixed)"
    );
    let rev1 = match &env_b1.payload {
        Some(envelope::Payload::StateDelta(d)) => {
            d.entities
                .iter()
                .find(|e| e.entity_id == entity.to_string())
                .expect("entity")
                .revision
        }
        _ => panic!("not delta"),
    };

    // Advance clock 2000ms and send again. Breaking sent_at to a constant
    // would make t2 == t1 and the next assert goes red.
    server.clock.advance_millis(2000);
    let t_expected = server.clock.now().to_unix_millis().expect("ts");
    let pos2 = orbisync_domain::Vec3::new(5.0, 0.0, 0.0).expect("vec");
    // Need to drain A's echo as well to keep ordering, but focus on B's timestamp.
    a.send(Message::Binary(
        transform_input_bytes(entity, rev1, pos2, 4).into(),
    ))
    .await
    .expect("send transform 2");
    let env_b2 = recv_delta_envelope(&mut b, entity).await;
    let t2 = env_b2.sent_at_unix_ms;
    assert_eq!(
        t2, t_expected,
        "second delivery sent_at must reflect advanced clock"
    );
    assert!(
        t2 > t1,
        "sent_at must be fresh per send: t2 ({t2}) > t1 ({t1}) — fixing sent_at to handshake time goes red"
    );
    assert_eq!(
        t2 - t1,
        2000,
        "advance of 2000ms must be visible in sent_at"
    );

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}
