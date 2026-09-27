//! W-18 Resume wiring E2E (server-stored token, D-23/D-24).
//!
//! Validates the 8 acceptance conditions via real WebSocket through
//! `realtime_ws_handler`. No `#[ignore]` tests.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orbisync_application::{SecretString, WorldDirectoryStore};
use orbisync_domain::{
    Clock as _, InstanceId, LoginId, Timestamp, Transform, World, WorldId, WorldInstance,
};
use orbisync_identity::{PasswordPolicy, PasswordService, token::AccessTokenService};
use orbisync_interest::UniformGrid;
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::{AccessTokenTicketVerifier, StubTicketVerifier};
use orbisync_server::{
    delivery::DeliveryRegistry,
    realtime_ws::{RealtimeState, realtime_ws_handler},
};
use orbisync_testkit::{FakeIdentityRepository, FakeWorldDirectoryStore, FixedClock};
use orbisync_world_runtime::RuntimeRegistry;
use prost::Message as ProstMessage;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

// ---------------------------------------------------------------------------
// constants / helpers
// ---------------------------------------------------------------------------

const PRIVATE_PEM: &[u8] = b"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIA1xcK2nctVkaHqStladAkbAg2dsR9j3I1r4gohGecsG\n-----END PRIVATE KEY-----\n";
const PUBLIC_PEM: &[u8] = b"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEArR8b3Nhj5pz4CNIZfW2T5gCOIykJul8FC7M1dsjhOJk=\n-----END PUBLIC KEY-----\n";

fn test_token_service() -> Arc<AccessTokenService> {
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
        "resume-world",
        None,
        Transform::identity(),
        100,
        now_ts(),
    )
    .expect("world")
}
fn make_instance(world_id: WorldId, capacity: u32) -> WorldInstance {
    WorldInstance::new(InstanceId::generate(), world_id, capacity, now_ts()).expect("instance")
}
fn encode(env: &Envelope) -> Vec<u8> {
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
}
fn decode(bytes: &[u8]) -> Envelope {
    Envelope::decode(bytes).expect("decode")
}
fn client_hello_bytes(ticket: &str, resume_token: &str) -> Vec<u8> {
    let hello = orbisync_protocol::v1::ClientHello {
        supported_minor_min: 0,
        supported_minor_max: 0,
        realtime_ticket: ticket.to_owned(),
        client_name: "resume-test".to_owned(),
        client_version: "0.1.0".to_owned(),
        client_type: "test".to_owned(),
        supported_compressions: Vec::new(),
        supported_features: Vec::new(),
        resume_token: resume_token.to_owned(),
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
    encode(&env)
}
fn join_bytes(instance_id: InstanceId) -> Vec<u8> {
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 2,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::JoinInstance(
            orbisync_protocol::v1::JoinInstance {
                world_instance_id: instance_id.to_string(),
            },
        )),
    };
    encode(&env)
}
fn resume_session_bytes(token: &str, last_applied: u64) -> Vec<u8> {
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 2,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::ResumeSession(
            orbisync_protocol::v1::ResumeSession {
                resume_token: token.to_owned(),
                last_applied_revision: last_applied,
                received_message_ids: Vec::new(),
            },
        )),
    };
    encode(&env)
}

#[tokio::test]
async fn resume_replays_messages_and_recovers_from_interrupted_replay() {
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (_, ticket) = create_user_and_ticket(
        tokens.clone(),
        clock.clone(),
        "replay_chat",
        "Cedar!Lake7-Comet",
    )
    .await;
    let world = make_world();
    let instance = make_instance(world.id(), 10);
    let iid = instance.id();
    let verifier = Arc::new(AccessTokenTicketVerifier::new(tokens, clock.clone()));
    let server = spawn_server_with_verifier(world, instance, verifier, clock).await;
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, &ticket, "").await;
    let (_, token, revision) = join_and_get_token(&mut a, iid).await;
    a.close(None).await.unwrap();
    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, &ticket, "").await;
    join_and_get_token(&mut b, iid).await;
    let mut sent = Vec::new();
    for seq in 3..5 {
        let id = uuid::Uuid::now_v7().to_string();
        let env = Envelope {
            protocol_major: PROTOCOL_MAJOR,
            sequence: seq,
            message_id: id.clone(),
            instance_id: iid.to_string(),
            payload: Some(envelope::Payload::DomainEvent(
                orbisync_protocol::v1::DomainEvent {
                    event_id: id,
                    event_type: "custom.chat.message".to_owned(),
                    ..Default::default()
                },
            )),
            ..Default::default()
        };
        b.send(Message::Binary(encode(&env).into())).await.unwrap();
        sent.push(recv_envelope(&mut b).await.unwrap());
    }
    let mut resumed = ws_connect(server.addr).await;
    handshake(&mut resumed, &ticket, &token).await;
    resumed
        .send(Message::Binary(
            resume_session_bytes(&token, revision).into(),
        ))
        .await
        .unwrap();
    let Some(envelope::Payload::ResumeAccepted(accepted)) =
        recv_envelope(&mut resumed).await.unwrap().payload
    else {
        panic!("resume");
    };
    let first = recv_envelope(&mut resumed).await.unwrap();
    assert_eq!(first.message_id, sent[0].message_id);
    let Some(envelope::Payload::DomainEvent(first_event)) = first.payload else {
        panic!("event");
    };
    // Simulate losing transport after applying only the first replayed event.
    drop(resumed);
    let mut again = ws_connect(server.addr).await;
    handshake(&mut again, &ticket, &accepted.resume_token).await;
    again
        .send(Message::Binary(
            resume_session_bytes(&accepted.resume_token, first_event.instance_revision).into(),
        ))
        .await
        .unwrap();
    assert!(matches!(
        recv_envelope(&mut again).await.unwrap().payload,
        Some(envelope::Payload::ResumeAccepted(_))
    ));
    let second = recv_envelope(&mut again).await.unwrap();
    assert_eq!(second.message_id, sent[1].message_id);
    assert_eq!(second.sequence, 3);
    assert!(matches!(
        recv_envelope(&mut again).await.unwrap().payload,
        Some(envelope::Payload::Snapshot(_))
    ));
    again.close(None).await.unwrap();
    // A later admission cannot request messages from before it joined by
    // claiming an older cursor, even with a valid token for the same user.
    let mut late = ws_connect(server.addr).await;
    handshake(&mut late, &ticket, "").await;
    let (_, late_token, _) = join_and_get_token(&mut late, iid).await;
    late.close(None).await.unwrap();
    let mut late_resume = ws_connect(server.addr).await;
    handshake(&mut late_resume, &ticket, &late_token).await;
    late_resume
        .send(Message::Binary(resume_session_bytes(&late_token, 0).into()))
        .await
        .unwrap();
    assert!(matches!(
        recv_envelope(&mut late_resume).await.unwrap().payload,
        Some(envelope::Payload::ResumeAccepted(_))
    ));
    assert!(matches!(
        recv_envelope(&mut late_resume).await.unwrap().payload,
        Some(envelope::Payload::Snapshot(_))
    ));
    late_resume.close(None).await.unwrap();
    b.close(None).await.unwrap();
}
fn transform_input_bytes(
    entity_id: orbisync_domain::EntityId,
    expected_rev: u64,
    pos: orbisync_domain::Vec3,
    sequence: u64,
) -> Vec<u8> {
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::TransformInput(
            orbisync_protocol::v1::TransformInput {
                entity_id: entity_id.to_string(),
                expected_revision: expected_rev,
                transform: Some(orbisync_protocol::v1::Transform {
                    position_x: pos.x(),
                    position_y: pos.y(),
                    position_z: pos.z(),
                    rotation_x: 0.0,
                    rotation_y: 0.0,
                    rotation_z: 0.0,
                    rotation_w: 1.0,
                }),
            },
        )),
    };
    encode(&env)
}

struct TestServer {
    addr: std::net::SocketAddr,
    _clock: Arc<FixedClock>,
    _store: Arc<FakeWorldDirectoryStore>,
    _registry: Arc<RuntimeRegistry>,
    _state: Arc<RealtimeState>,
}

async fn spawn_server_with_verifier(
    world: World,
    instance: WorldInstance,
    verifier: Arc<dyn orbisync_realtime::gateway::RealtimeTicketVerifier>,
    clock: Arc<FixedClock>,
) -> TestServer {
    let store = Arc::new(FakeWorldDirectoryStore::new());
    let world_id = world.id();
    let instance_id = instance.id();
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(store.clone() as Arc<dyn WorldDirectoryStore>)
        .with_tickets(verifier)
        .with_checkpoint_store(
            Arc::new(common::NoopCheckpointStore) as Arc<dyn orbisync_application::CheckpointStore>
        )
        .with_world_authorizer(Arc::new(orbisync_testkit::AllowAllAuthorizer))
        .with_interest_grid(grid)
        // Use short grace for tests so expiry can be tested without 60s wait,
        // but still >500ms so normal resume succeeds.
        .with_resume_grace_seconds(60)
        .build(),
    );
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(Arc::clone(&state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move { axum::serve(listener, app).await.expect("serve") });
    tokio::time::sleep(Duration::from_millis(50)).await;
    // sanity
    let got = store
        .get_instance(instance_id)
        .await
        .expect("get")
        .expect("present");
    assert_eq!(got.world_id(), world_id);
    TestServer {
        addr,
        _clock: clock,
        _store: store,
        _registry: registry,
        _state: state,
    }
}

async fn ws_connect(
    addr: std::net::SocketAddr,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let uri = format!("ws://{addr}/ws").try_into().expect("uri");
    let req = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    tokio_tungstenite::connect_async(req)
        .await
        .expect("ws connect")
        .0
}
async fn recv_envelope<S>(ws: &mut S) -> Option<Envelope>
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .expect("timeout")?;
        let msg = msg.expect("ws recv");
        match msg {
            Message::Binary(data) => {
                let env = decode(&data);
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
async fn handshake(
    ws: &mut (
             impl SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
             + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
             + Unpin
         ),
    ticket: &str,
    resume_hint: &str,
) {
    let hello = client_hello_bytes(ticket, resume_hint);
    ws.send(Message::Binary(hello.into())).await.expect("hello");
    let env = recv_envelope(ws).await.expect("ServerHello");
    assert!(matches!(
        env.payload,
        Some(envelope::Payload::ServerHello(_))
    ));
}
async fn join_and_get_token<S>(ws: &mut S, instance_id: InstanceId) -> (String, String, u64)
where
    S: SinkExt<Message, Error = tokio_tungstenite::tungstenite::Error>
        + StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    ws.send(Message::Binary(join_bytes(instance_id).into()))
        .await
        .expect("join");
    let first = recv_envelope(ws).await.expect("join resp 1");
    let second = recv_envelope(ws).await.expect("join resp 2");
    // Determine which is JoinAccepted
    let (accepted, snapshot) = if matches!(first.payload, Some(envelope::Payload::JoinAccepted(_)))
    {
        (first, second)
    } else {
        (second, first)
    };
    let (presence_id, revision, token) = match accepted.payload {
        Some(envelope::Payload::JoinAccepted(j)) => {
            (j.presence_id, j.instance_revision, j.resume_token)
        }
        other => panic!("expected JoinAccepted, got {other:?}"),
    };
    match snapshot.payload {
        Some(envelope::Payload::Snapshot(_)) => {}
        other => panic!("expected Snapshot, got {other:?}"),
    }
    (presence_id, token, revision)
}

// Helper to create alice/bob repo and issue real tickets
async fn create_user_and_ticket(
    token_service: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
    login_id: &str,
    password: &str,
) -> (Arc<FakeIdentityRepository>, String) {
    let passwords = Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("pwd"));
    let repo = Arc::new(FakeIdentityRepository::new());
    let hash = passwords
        .hash(SecretString::new(password.to_owned()))
        .await
        .expect("hash");
    let user_id = orbisync_domain::UserId::generate();
    let lid = LoginId::new(login_id.to_owned()).expect("login");
    let user = orbisync_domain::User::new(user_id, lid, login_id, now_ts()).expect("user");
    let cred = orbisync_domain::Credential::new(user_id, hash, now_ts());
    repo.insert_account(orbisync_application::LoginAccount {
        user: user.clone(),
        credential: cred,
    });
    // Simulate login: directly issue token + ticket via service
    // We need session id
    let session_id = orbisync_domain::AuthSessionId::generate();
    let access = token_service
        .issue(user_id, session_id, clock.now())
        .expect("issue");
    let _claims = token_service
        .validate(&access, clock.now())
        .expect("validate");
    let ticket = token_service
        .issue_realtime_ticket(user_id, session_id, clock.now())
        .expect("ticket");
    (repo, ticket.expose_secret().to_owned())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w18_join_returns_resume_token() {
    let clock = fixed_clock();
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    let server =
        spawn_server_with_verifier(world, instance, Arc::new(StubTicketVerifier::new()), clock)
            .await;
    let mut ws = ws_connect(server.addr).await;
    handshake(&mut ws, "stub-ticket-123", "").await;
    let (presence, token, rev) = join_and_get_token(&mut ws, iid).await;
    assert!(
        !token.is_empty(),
        "JoinAccepted.resume_token must not be empty (condition 1)"
    );
    assert!(!presence.is_empty());
    assert!(rev >= 1);
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = ws.close(None).await;
}

#[tokio::test]
async fn w18_resume_same_presence_and_replay() {
    // Condition 2: same PresenceId and missed changes arrive. Use real verifier so same ticket yields same UserId.
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (repo, ticket_a) =
        create_user_and_ticket(tokens.clone(), clock.clone(), "alice2", "Cedar!Lake7-Comet").await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = repo;
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    let verifier = Arc::new(AccessTokenTicketVerifier::new(
        tokens.clone(),
        clock.clone() as Arc<dyn orbisync_domain::Clock>,
    ));
    let server = spawn_server_with_verifier(world, instance, verifier, clock.clone()).await;

    // A joins
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, &ticket_a, "").await;
    let (presence_a, token_a, rev_a) = join_and_get_token(&mut a, iid).await;
    assert!(!token_a.is_empty());

    // A disconnects (goes into grace, PresenceGuard keeps membership)
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // While A is in grace, B joins with different user (bob)
    let (_repo_b, ticket_b) =
        create_user_and_ticket(tokens.clone(), clock.clone(), "bob2", "Cedar!Lake7-Comet2").await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, &ticket_b, "").await;
    let (_pres_b, _tok_b, _rev_b) = join_and_get_token(&mut b, iid).await;
    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    // B creates entity via TransformInput (auto-create avatar-style)
    b.send(Message::Binary(
        transform_input_bytes(entity, 1, pos, 3).into(),
    ))
    .await
    .expect("transform");
    // B should get its own delta
    let env_b = recv_envelope(&mut b).await.expect("b delta");
    assert!(matches!(
        env_b.payload,
        Some(envelope::Payload::StateDelta(_))
    ));
    // Give server time to apply and bump revision
    tokio::time::sleep(Duration::from_millis(100)).await;

    // A resumes with new connection, same ticket and token, last_applied = rev_a
    let mut a2 = ws_connect(server.addr).await;
    handshake(&mut a2, &ticket_a, &token_a).await;
    // Send ResumeSession
    a2.send(Message::Binary(
        resume_session_bytes(&token_a, rev_a).into(),
    ))
    .await
    .expect("resume");
    // Expect ResumeAccepted with same presence
    let env = recv_envelope(&mut a2).await.expect("resume accepted");
    let (resumed_presence, resumed_token, replay) = match env.payload {
        Some(envelope::Payload::ResumeAccepted(r)) => {
            (r.presence_id, r.resume_token, r.replay_follows)
        }
        other => panic!("expected ResumeAccepted, got {other:?} condition 2 fail"),
    };
    assert_eq!(
        resumed_presence, presence_a,
        "must resume same PresenceId (condition 2)"
    );
    assert!(
        replay,
        "replay_follows must be true for within-window resume"
    );
    assert!(!resumed_token.is_empty());
    // Following should be snapshot/replay containing the entity B created
    // We sent snapshot as replay; check it contains the entity.
    let replay_env = recv_envelope(&mut a2).await.expect("replay snapshot");
    let has_entity = match replay_env.payload {
        Some(envelope::Payload::Snapshot(s)) => {
            let txt = String::from_utf8_lossy(&s.data);
            txt.contains(&entity.to_string())
        }
        Some(envelope::Payload::StateDelta(d)) => {
            d.entities.iter().any(|e| e.entity_id == entity.to_string())
        }
        other => panic!("expected replay Snapshot/StateDelta, got {other:?}"),
    };
    assert!(
        has_entity,
        "replayed state must contain entity created during disconnect (condition 2)"
    );

    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a2.close(None).await;
}

#[tokio::test]
async fn w18_resume_token_rotates() {
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (repo, ticket) = create_user_and_ticket(
        tokens.clone(),
        clock.clone(),
        "alice_rot",
        "Cedar!Lake7-Comet",
    )
    .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = repo;
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    let verifier = Arc::new(AccessTokenTicketVerifier::new(
        tokens.clone(),
        clock.clone() as Arc<dyn orbisync_domain::Clock>,
    ));
    let server = spawn_server_with_verifier(world, instance, verifier, clock.clone()).await;
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, &ticket, "").await;
    let (_pres, token, rev) = join_and_get_token(&mut a, iid).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut a2 = ws_connect(server.addr).await;
    handshake(&mut a2, &ticket, &token).await;
    a2.send(Message::Binary(resume_session_bytes(&token, rev).into()))
        .await
        .expect("resume");
    let env = recv_envelope(&mut a2).await.expect("accepted");
    let new_token = match env.payload {
        Some(envelope::Payload::ResumeAccepted(r)) => r.resume_token,
        other => panic!("expected ResumeAccepted, got {other:?}"),
    };
    assert_ne!(
        new_token, token,
        "rotated token must differ from consumed token (condition 3)"
    );
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a2.close(None).await;
}

#[tokio::test]
async fn w18_single_use_token_rejected() {
    let clock = fixed_clock();
    let tokens = test_token_service();
    let (repo, ticket) = create_user_and_ticket(
        tokens.clone(),
        clock.clone(),
        "alice_single",
        "Cedar!Lake7-Comet",
    )
    .await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = repo;
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    let verifier = Arc::new(AccessTokenTicketVerifier::new(
        tokens.clone(),
        clock.clone() as Arc<dyn orbisync_domain::Clock>,
    ));
    let server = spawn_server_with_verifier(world, instance, verifier, clock.clone()).await;
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, &ticket, "").await;
    let (_pres, token, rev) = join_and_get_token(&mut a, iid).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // First resume succeeds
    let mut a2 = ws_connect(server.addr).await;
    handshake(&mut a2, &ticket, &token).await;
    a2.send(Message::Binary(resume_session_bytes(&token, rev).into()))
        .await
        .expect("resume");
    let env = recv_envelope(&mut a2).await.expect("accepted");
    assert!(matches!(
        env.payload,
        Some(envelope::Payload::ResumeAccepted(_))
    ));
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a2.close(None).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Second use of same token must be rejected -> ResyncRequired (not ResumeAccepted)
    let mut a3 = ws_connect(server.addr).await;
    handshake(&mut a3, &ticket, &token).await;
    a3.send(Message::Binary(resume_session_bytes(&token, rev).into()))
        .await
        .expect("second resume");
    let env2 = recv_envelope(&mut a3).await.expect("second response");
    match env2.payload {
        Some(envelope::Payload::ResyncRequired(_)) => {}
        Some(envelope::Payload::ResumeAccepted(_)) => {
            panic!("single-use: second use must not be ResumeAccepted (condition 4)")
        }
        other => panic!("expected ResyncRequired for single-use violation, got {other:?}"),
    }
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a3.close(None).await;
}

#[tokio::test]
async fn w18_expired_token_resync_required() {
    let clock = fixed_clock();
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    // Need real grace handling: use server with clock we can advance
    let server = spawn_server_with_verifier(
        world,
        instance,
        Arc::new(StubTicketVerifier::new()),
        clock.clone(),
    )
    .await;
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, "stub-ticket", "").await;
    let (_pres, token, rev) = join_and_get_token(&mut a, iid).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Advance beyond grace (60s) + skew
    clock.advance_millis(61_000);
    let mut a2 = ws_connect(server.addr).await;
    handshake(&mut a2, "stub-ticket", &token).await;
    a2.send(Message::Binary(resume_session_bytes(&token, rev).into()))
        .await
        .expect("resume expired");
    let env = recv_envelope(&mut a2).await.expect("resp");
    match env.payload {
        Some(envelope::Payload::ResyncRequired(r)) => {
            assert!(!r.reason_code.is_empty());
        }
        other => panic!("expired token must yield ResyncRequired (condition 5), got {other:?}"),
    }
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a2.close(None).await;
}

#[tokio::test]
async fn w18_gap_resync_required() {
    let clock = fixed_clock();
    // Use deterministic fixed user verifier to avoid ticket expiry during long gap fill.
    let fixed_user = orbisync_domain::UserId::generate();
    struct FixedUserVerifier {
        user_id: orbisync_domain::UserId,
    }
    #[async_trait::async_trait]
    impl orbisync_realtime::gateway::RealtimeTicketVerifier for FixedUserVerifier {
        async fn verify(
            &self,
            ticket: &str,
        ) -> Result<orbisync_domain::UserId, orbisync_realtime::gateway::GatewayError> {
            if ticket.is_empty() {
                Err(orbisync_realtime::gateway::GatewayError::InvalidTicket)
            } else {
                Ok(self.user_id)
            }
        }
    }
    let verifier = Arc::new(FixedUserVerifier {
        user_id: fixed_user,
    });
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    let server = spawn_server_with_verifier(
        world,
        instance,
        verifier.clone() as Arc<dyn orbisync_realtime::gateway::RealtimeTicketVerifier>,
        clock.clone(),
    )
    .await;
    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, "fixed-ticket", "").await;
    let _ticket = "fixed-ticket".to_string();
    let (_pres, token, _rev) = join_and_get_token(&mut a, iid).await;

    // Evict actual reliable payloads. Latest-wins transforms do not occupy
    // reliable replay history, and this fixture has no persistence drainer.
    let mut b = ws_connect(server.addr).await;
    handshake(&mut b, "fixed-ticket", "").await;
    let (pres_b, _, _) = join_and_get_token(&mut b, iid).await;
    let presence_id = orbisync_domain::PresenceId::parse(&pres_b).unwrap();
    for _ in 0..257 {
        let outcome = server
            ._state
            .registry
            .submit(
                iid,
                orbisync_world_runtime::command::InstanceCommand::PublishEvent {
                    presence_id,
                    user_id: fixed_user,
                },
            )
            .await
            .unwrap();
        let orbisync_world_runtime::command::CommandOutcome::Applied { revision, .. } = outcome
        else {
            panic!("history setup rejected");
        };
        server
            ._state
            .registry
            .submit(
                iid,
                orbisync_world_runtime::command::InstanceCommand::RetainReliable {
                    message_id: revision.as_u64().to_string(),
                    revision,
                    payload: Arc::from([1_u8].as_slice()),
                },
            )
            .await
            .unwrap();
    }
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;

    tokio::time::sleep(Duration::from_millis(50)).await;
    // Now try resume with very old last_applied (1) which is outside window (oldest ~ rev_b-256)
    let mut a2 = ws_connect(server.addr).await;
    handshake(&mut a2, "fixed-ticket", &token).await;
    a2.send(Message::Binary(resume_session_bytes(&token, 1).into()))
        .await
        .expect("resume gap");
    let env = recv_envelope(&mut a2).await.expect("resp");
    match env.payload {
        Some(envelope::Payload::ResyncRequired(r)) => {
            assert!(
                r.reason_code == "revision_gap" || r.reason_code == "history_unavailable",
                "gap must be ResyncRequired (condition 6), got {}",
                r.reason_code
            );
        }
        other => panic!("gap must yield ResyncRequired, got {other:?}"),
    }
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a2.close(None).await;
}

#[tokio::test]
async fn w18_resume_requires_ticket() {
    // Condition 7: invalid ticket + valid resume token -> rejected
    let clock = fixed_clock();
    let _tokens = test_token_service();
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();
    // First create a valid resume token via stub path
    let server1 = spawn_server_with_verifier(
        world.clone(),
        instance.clone(),
        Arc::new(StubTicketVerifier::new()),
        clock.clone(),
    )
    .await;
    let mut a = ws_connect(server1.addr).await;
    handshake(&mut a, "valid-ticket", "").await;
    let (_pres, token, rev) = join_and_get_token(&mut a, iid).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    // Need server with real verifier for second connection: invalid ticket should be rejected at handshake
    let world2 = make_world();
    // Use same instance id? Need fresh server with real verifier
    // For this test we reuse the same store but need real verifier - create new server
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world2.clone());
    let inst2 = make_instance(world2.id(), 100);
    let _iid2 = inst2.id();
    store.insert_instance(inst2.clone());
    // Issue a valid resume token for this new instance via stub? Instead reuse token from previous server? That token won't match iid2.
    // Instead we test stub case: invalid ticket (empty) + valid token from first server should be AUTHENTICATION_REQUIRED before resume.
    // Use real verifier server for this check: empty ticket is invalid.
    let server2 = spawn_server_with_verifier(
        world2,
        inst2,
        Arc::new(AccessTokenTicketVerifier::new(
            test_token_service(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
        )),
        clock.clone(),
    )
    .await;
    let mut evil = ws_connect(server2.addr).await;
    // Send hello with empty ticket (invalid) but hint resume token
    let hello = client_hello_bytes("invalid-jwt", &token);
    evil.send(Message::Binary(hello.into()))
        .await
        .expect("hello");
    // Server should send ServerHello then AUTHENTICATION_REQUIRED error (ticket verify fails)
    // First recv is ServerHello
    let first = recv_envelope(&mut evil).await.expect("first");
    // May be ServerHello then Error, or directly Error? With real verifier, handshake still succeeds to ServerHello then ticket verify fails after.
    let mut got_auth_err = false;
    if matches!(first.payload, Some(envelope::Payload::ServerHello(_))) {
        let second = recv_envelope(&mut evil).await.expect("second");
        if let Some(envelope::Payload::Error(e)) = second.payload
            && e.code == "AUTHENTICATION_REQUIRED"
        {
            got_auth_err = true;
        }
    } else if let Some(envelope::Payload::Error(e)) = first.payload
        && e.code == "AUTHENTICATION_REQUIRED"
    {
        got_auth_err = true;
    }
    assert!(
        got_auth_err,
        "invalid ticket + valid resume token must be rejected with AUTHENTICATION_REQUIRED (condition 7)"
    );
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = evil.close(None).await;

    // Also test that valid ticket path still works for same token length but invalid ticket is empty.
    // The key point is resume does not bypass ticket verification.
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = rev; // suppress unused
}

#[tokio::test]
async fn w18_other_users_token_rejected() {
    let clock = fixed_clock();
    let tokens = test_token_service();
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let iid = instance.id();

    // Create alice ticket + resume token
    let (repo_alice, ticket_alice) =
        create_user_and_ticket(tokens.clone(), clock.clone(), "alice", "Cedar!Lake7-Comet").await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = repo_alice;
    let server = spawn_server_with_verifier(
        world.clone(),
        instance.clone(),
        Arc::new(AccessTokenTicketVerifier::new(
            tokens.clone(),
            clock.clone() as Arc<dyn orbisync_domain::Clock>,
        )),
        clock.clone(),
    )
    .await;
    let mut alice = ws_connect(server.addr).await;
    handshake(&mut alice, &ticket_alice, "").await;
    let (_pres_alice, token_alice, rev_alice) = join_and_get_token(&mut alice, iid).await;
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = alice.close(None).await;
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Bob
    let (_repo_bob, ticket_bob) =
        create_user_and_ticket(tokens.clone(), clock.clone(), "bob", "Cedar!Lake7-Comet2").await;
    let mut bob = ws_connect(server.addr).await;
    handshake(&mut bob, &ticket_bob, &token_alice).await;
    // Bob tries to resume with alice's token
    bob.send(Message::Binary(
        resume_session_bytes(&token_alice, rev_alice).into(),
    ))
    .await
    .expect("bob resume with alice token");
    let env = recv_envelope(&mut bob).await.expect("bob resp");
    match env.payload {
        Some(envelope::Payload::ResyncRequired(_)) => {}
        Some(envelope::Payload::ResumeAccepted(_)) => {
            panic!("other's token must not be accepted (condition 8)")
        }
        other => panic!("expected ResyncRequired for other's token, got {other:?}"),
    }
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = bob.close(None).await;

    // Rejecting a different user must not consume Alice's single-use token.
    let mut alice_again = ws_connect(server.addr).await;
    handshake(&mut alice_again, &ticket_alice, &token_alice).await;
    alice_again
        .send(Message::Binary(
            resume_session_bytes(&token_alice, rev_alice).into(),
        ))
        .await
        .expect("alice resumes after rejected attempt");
    let response = recv_envelope(&mut alice_again)
        .await
        .expect("alice response");
    assert!(
        matches!(response.payload, Some(envelope::Payload::ResumeAccepted(_))),
        "the rightful owner's token must remain usable: {:?}",
        response.payload
    );
    alice_again.close(None).await.expect("close alice");
}
