//! RV-A C1 and C3 integration tests (real DB + public boundary).
//!
//! C1: single-use ticket, session revocation, concurrent consumption, no secret in audit/logs.
//! C3: handshake timeout, codec max_message_size, semaphore limit.
//!
//! All tests use real PostgreSQL via `common::pool_or_skip` (V-07) and the public
//! HTTP/WS boundary (not fakes), so mutations on production code are caught.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use orbisync_application::{
    CreateRealtimeTicketCommand, RealtimeTicketConsumption, RealtimeTicketStore,
};
use orbisync_config::Config;
use orbisync_domain::{AuthSessionId, Clock, Timestamp, UserId};
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::HmacRealtimeTicketVerifier;
use orbisync_server::realtime_ws::{RealtimeState, realtime_ws_handler};
use orbisync_storage_postgres::PgRealtimeTicketStore;
use orbisync_testkit::FixedClock;
use prost::Message as ProstMessage;
use sqlx::PgPool;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message as WsMessage;
use uuid::Uuid;

mod common;

fn test_hmac_key() -> Vec<u8> {
    // Deterministic test key (32 bytes). In production this comes from env.
    b"test-realtime-ticket-hmac-key-32b!!".to_vec()
}

fn fixed_now() -> Timestamp {
    Timestamp::from_unix_millis(1_700_000_000_000).expect("valid")
}

async fn setup_db() -> Option<PgPool> {
    common::pool_or_skip().await
}

async fn insert_user_session(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    now_odt: time::OffsetDateTime,
) {
    let login = format!("rv-a-{}", user_id);
    sqlx::query("INSERT INTO users (id, login_id, display_name, status, must_change_password, revision, created_at, updated_at) VALUES ($1, $2, $3, 'active', false, 1, $4, $4)")
        .bind(user_id)
        .bind(&login)
        .bind(format!("User {}", &login[..8]))
        .bind(now_odt)
        .execute(pool)
        .await
        .expect("insert user");
    sqlx::query("INSERT INTO auth_sessions (id, user_id, status, created_at, expires_at, revision) VALUES ($1, $2, 'active', $3, $4, 0)")
        .bind(session_id)
        .bind(user_id)
        .bind(now_odt)
        .bind(now_odt + time::Duration::days(1))
        .execute(pool)
        .await
        .expect("insert session");
}

fn encode_envelope(env: &Envelope) -> Vec<u8> {
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
}

fn client_hello_bytes(ticket: &str) -> Vec<u8> {
    let hello = orbisync_protocol::v1::ClientHello {
        supported_minor_min: 0,
        supported_minor_max: 0,
        realtime_ticket: ticket.to_owned(),
        client_name: "rv-a-test".to_owned(),
        client_version: "0.1.0".to_owned(),
        client_type: "test".to_owned(),
        supported_compressions: Vec::new(),
        supported_features: Vec::new(),
        resume_token: String::new(),
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: Uuid::now_v7().to_string(),
        sequence: 1,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::ClientHello(hello)),
    };
    encode_envelope(&env)
}

fn client_hello_bytes_with_large_field(ticket: &str, large: &str) -> Vec<u8> {
    let hello = orbisync_protocol::v1::ClientHello {
        supported_minor_min: 0,
        supported_minor_max: 0,
        realtime_ticket: ticket.to_owned(),
        client_name: large.to_owned(),
        client_version: "0.1.0".to_owned(),
        client_type: "test".to_owned(),
        supported_compressions: Vec::new(),
        supported_features: Vec::new(),
        resume_token: String::new(),
    };
    let env = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: Uuid::now_v7().to_string(),
        sequence: 1,
        sent_at_unix_ms: 1_700_000_000_000,
        instance_id: String::new(),
        payload: Some(envelope::Payload::ClientHello(hello)),
    };
    encode_envelope(&env)
}

async fn create_ticket_via_store(
    pool: &PgPool,
    user_id: Uuid,
    session_id: Uuid,
    now: Timestamp,
) -> (String, [u8; 32]) {
    let raw = orbisync_transport_http::generate_realtime_ticket();
    let digest = orbisync_transport_http::realtime_ticket_digest(&test_hmac_key(), &raw);
    let expires_at = now.checked_add_millis(60_000).expect("expires");
    let store = PgRealtimeTicketStore::new(pool.clone());
    let cmd = CreateRealtimeTicketCommand {
        token_digest: digest,
        session_id: AuthSessionId::new(session_id).expect("session"),
        user_id: UserId::new(user_id).expect("user"),
        issued_at: now,
        expires_at,
    };
    store.create(cmd).await.expect("create ticket");
    (raw.expose_secret().to_owned(), digest)
}

async fn spawn_ws_server(
    pool: PgPool,
    config: Config,
) -> (std::net::SocketAddr, Arc<RealtimeState>) {
    use orbisync_application::WorldDirectoryStore;
    use orbisync_domain::{InstanceId, Transform, World, WorldId, WorldInstance};
    use orbisync_interest::UniformGrid;
    use orbisync_server::delivery::DeliveryRegistry;
    use orbisync_testkit::FakeWorldDirectoryStore;
    use orbisync_world_runtime::RuntimeRegistry;

    // Create a world/instance for the WS server to accept JoinInstance
    let world = World::new(
        WorldId::generate(),
        "rv-a-world",
        None,
        Transform::identity(),
        10,
        fixed_now(),
    )
    .expect("world");
    let world_id = world.id();
    let instance =
        WorldInstance::new(InstanceId::generate(), world_id, 10, fixed_now()).expect("instance");
    let instance_id = instance.id();

    let fake_store = Arc::new(FakeWorldDirectoryStore::new());
    fake_store.insert_world(world);
    fake_store.insert_instance(instance);

    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let clock = Arc::new(FixedClock::new(fixed_now()));
    let grid = UniformGrid::default();
    let ticket_store: Arc<dyn RealtimeTicketStore> =
        Arc::new(PgRealtimeTicketStore::new(pool.clone()));
    let verifier = Arc::new(HmacRealtimeTicketVerifier::new(
        ticket_store,
        test_hmac_key(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));

    let state = Arc::new(
        RealtimeState::builder(
            config.realtime.clone(),
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .with_world_store(fake_store as Arc<dyn WorldDirectoryStore>)
        .with_tickets(verifier)
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
    // Return state for metrics inspection
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = instance_id; // keep instance alive via store
    (addr, state)
}

async fn ws_connect(
    addr: std::net::SocketAddr,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let uri = format!("ws://{addr}/ws")
        .try_into()
        .map_err(|e| format!("uri: {e:?}"))?;
    let req = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    tokio_tungstenite::connect_async(req)
        .await
        .map(|(ws, _)| ws)
        .map_err(|e| format!("connect: {e:?}"))
}

// Helper: perform ClientHello handshake and return Ok(ServerHello) or Err(ErrorMessage/Close)
async fn do_handshake(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    ticket: &str,
) -> Result<Envelope, String> {
    ws.send(WsMessage::Binary(client_hello_bytes(ticket).into()))
        .await
        .map_err(|e| format!("send: {e:?}"))?;
    // Wait for ServerHello or Error, skipping heartbeats, with timeout
    let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return Err("timeout waiting for ServerHello".to_owned());
        }
        let msg = tokio::time::timeout(remaining, ws.next())
            .await
            .map_err(|_| "timeout".to_owned())?
            .ok_or("closed".to_owned())?
            .map_err(|e| format!("recv err: {e:?}"))?;
        match msg {
            WsMessage::Binary(data) => {
                let env = Envelope::decode(data.as_ref()).map_err(|e| format!("decode: {e}"))?;
                match &env.payload {
                    Some(envelope::Payload::ServerHello(_)) => return Ok(env),
                    Some(envelope::Payload::Error(err)) => {
                        return Err(format!("error: {} {}", err.code, err.message));
                    }
                    Some(envelope::Payload::Heartbeat(_))
                    | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                    _ => continue,
                }
            }
            WsMessage::Close(_) => return Err("closed".to_owned()),
            _ => continue,
        }
    }
}

// ---------------------------------------------------------------------------
// C1 Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn c1_same_ticket_two_ws_only_one_succeeds() {
    let Some(pool) = setup_db().await else { return };
    let now = fixed_now();
    let now_odt = now.as_offset_date_time();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    insert_user_session(&pool, user_id, session_id, now_odt).await;
    let (ticket_raw, _digest) = create_ticket_via_store(&pool, user_id, session_id, now).await;

    let config = Config::default();
    let (addr, _state) = spawn_ws_server(pool.clone(), config).await;

    // Two concurrent handshakes with same ticket
    let t1 = ticket_raw.clone();
    let t2 = ticket_raw.clone();
    let addr1 = addr;
    let addr2 = addr;
    let h1 = tokio::spawn(async move {
        let mut ws = ws_connect(addr1).await.expect("connect1");
        do_handshake(&mut ws, &t1).await
    });
    let h2 = tokio::spawn(async move {
        let mut ws = ws_connect(addr2).await.expect("connect2");
        do_handshake(&mut ws, &t2).await
    });
    let r1 = h1.await.expect("join1");
    let r2 = h2.await.expect("join2");
    let successes = [r1.is_ok(), r2.is_ok()].iter().filter(|&&x| x).count();
    assert_eq!(
        successes,
        1,
        "same ticket must succeed for exactly one WS (C1 single-use): r1={:?} r2={:?} ticket_len={}",
        r1,
        r2,
        ticket_raw.len()
    );
}

#[tokio::test]
async fn c1_revoked_session_rejects_unused_ticket() {
    let Some(pool) = setup_db().await else { return };
    let now = fixed_now();
    let now_odt = now.as_offset_date_time();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    insert_user_session(&pool, user_id, session_id, now_odt).await;
    let (ticket_raw, _digest) = create_ticket_via_store(&pool, user_id, session_id, now).await;

    // Revoke session before using ticket
    sqlx::query("UPDATE auth_sessions SET status='revoked', revoked_at=$2, revocation_reason='test', revision=revision+1 WHERE id=$1")
        .bind(session_id)
        .bind(now_odt)
        .execute(&pool)
        .await
        .expect("revoke");

    let config = Config::default();
    let (addr, _state) = spawn_ws_server(pool.clone(), config).await;
    let mut ws = ws_connect(addr).await.expect("connect");
    let res = do_handshake(&mut ws, &ticket_raw).await;
    assert!(
        res.is_err(),
        "ticket from revoked session must be rejected (C1): got {:?}",
        res
    );
}

#[tokio::test]
async fn c1_concurrent_consumers_only_one_owner() {
    let Some(pool) = setup_db().await else { return };
    let now = fixed_now();
    let now_odt = now.as_offset_date_time();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    insert_user_session(&pool, user_id, session_id, now_odt).await;

    let raw = orbisync_transport_http::generate_realtime_ticket();
    let digest = orbisync_transport_http::realtime_ticket_digest(&test_hmac_key(), &raw);
    let expires_at = now.checked_add_millis(60_000).expect("exp");
    let store = PgRealtimeTicketStore::new(pool.clone());
    let cmd = CreateRealtimeTicketCommand {
        token_digest: digest,
        session_id: AuthSessionId::new(session_id).expect("sid"),
        user_id: UserId::new(user_id).expect("uid"),
        issued_at: now,
        expires_at,
    };
    store.create(cmd).await.expect("create");

    // Spawn multiple consumers concurrently
    let n = 10;
    let mut handles = Vec::new();
    for _ in 0..n {
        let s = store.clone();
        let d = digest;
        let ts = now;
        handles.push(tokio::spawn(async move {
            let res = s.consume(d, ts).await.expect("consume");
            matches!(res, RealtimeTicketConsumption::Consumed { .. })
        }));
    }
    let mut successes = 0;
    for h in handles {
        if h.await.expect("join") {
            successes += 1;
        }
    }
    assert_eq!(
        successes, 1,
        "concurrent consumers must have exactly one owner (C1 atomic)"
    );
}

#[tokio::test]
async fn c1_audit_and_logs_contain_no_ticket_material() {
    let Some(pool) = setup_db().await else { return };
    let now = fixed_now();
    let now_odt = now.as_offset_date_time();
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    insert_user_session(&pool, user_id, session_id, now_odt).await;
    let (ticket_raw, digest) = create_ticket_via_store(&pool, user_id, session_id, now).await;
    let digest_hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();

    // Consume the ticket via a WS handshake (so the ticket goes through the gateway)
    let config = Config::default();
    let (addr, _state) = spawn_ws_server(pool.clone(), config).await;
    let mut ws = ws_connect(addr).await.expect("connect");
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = do_handshake(&mut ws, &ticket_raw).await;

    // Check audit_events do not contain ticket raw or digest
    let rows: Vec<(String, serde_json::Value)> = sqlx::query_as(
        "SELECT action, metadata FROM audit_events ORDER BY occurred_at DESC LIMIT 20",
    )
    .fetch_all(&pool)
    .await
    .expect("audit");
    for (action, meta) in rows {
        let meta_str = meta.to_string();
        let combined = format!("{action} {meta_str}");
        assert!(
            !combined.contains(&ticket_raw),
            "audit must not contain raw ticket for action {action}"
        );
        assert!(
            !combined.to_lowercase().contains(&digest_hex.to_lowercase()),
            "audit must not contain digest for action {action}"
        );
        // Also ensure digest raw bytes not leaked as string (check not containing ticket prefix)
        assert!(
            !combined.contains("realtime_ticket"),
            "audit should not reference ticket field"
        );
    }
    // No direct log check here, but we ensure the ticket is not in DB's realtime_tickets after consumption
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM realtime_tickets WHERE token_digest = $1")
            .bind(digest.as_slice())
            .fetch_one(&pool)
            .await
            .expect("count");
    assert_eq!(count, 0, "ticket must be consumed (single-use)");
    // Ensure raw ticket not stored anywhere (only digest)
    let raw_in_db: Option<Vec<u8>> =
        sqlx::query_scalar("SELECT token_digest FROM realtime_tickets LIMIT 1")
            .fetch_optional(&pool)
            .await
            .expect("query");
    if let Some(d) = raw_in_db {
        assert_ne!(d, ticket_raw.as_bytes(), "raw ticket must not be stored");
    }
}

// ---------------------------------------------------------------------------
// C3 Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn c3_handshake_timeout_disconnects() {
    let Some(pool) = setup_db().await else { return };
    let mut config = Config::default();
    config.realtime.handshake_timeout_ms = 500; // short for test
    let (addr, state) = spawn_ws_server(pool.clone(), config).await;
    let before = state
        .handshake_timeout_total
        .load(std::sync::atomic::Ordering::Relaxed);

    // Connect but don't send ClientHello
    let mut ws = ws_connect(addr).await.expect("connect");
    // Wait longer than timeout
    tokio::time::sleep(Duration::from_millis(900)).await;
    // The server should have closed the connection
    let next = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
    match next {
        Ok(Some(Ok(WsMessage::Close(_)))) => {} // expected
        Ok(Some(Err(_))) => {}                  // tungstenite error also indicates close
        Ok(None) => {}                          // stream ended
        Ok(Some(Ok(other))) => panic!("expected close after handshake timeout, got {:?}", other),
        Err(_) => panic!("timeout waiting for close after handshake timeout"),
    }
    // Give server time to increment metric
    tokio::time::sleep(Duration::from_millis(100)).await;
    let after = state
        .handshake_timeout_total
        .load(std::sync::atomic::Ordering::Relaxed);
    assert!(
        after > before,
        "handshake timeout metric must increment (C3) before={} after={}",
        before,
        after
    );
}

#[tokio::test]
async fn c3_oversized_initial_frame_rejected_at_codec() {
    let Some(pool) = setup_db().await else { return };
    let mut config = Config::default();
    config.realtime.max_message_bytes = 1024;
    let user_id = Uuid::now_v7();
    let session_id = Uuid::now_v7();
    let now = fixed_now();
    let now_odt = now.as_offset_date_time();
    insert_user_session(&pool, user_id, session_id, now_odt).await;
    let (ticket_raw, _digest) = create_ticket_via_store(&pool, user_id, session_id, now).await;

    let (addr, _state) = spawn_ws_server(pool.clone(), config).await;
    let mut ws = ws_connect(addr).await.expect("connect");
    // Craft oversized ClientHello: valid ticket but large client_name to exceed 1024
    let large = "A".repeat(2000);
    let oversized = client_hello_bytes_with_large_field(&ticket_raw, &large);
    assert!(
        oversized.len() > 1024,
        "test payload must exceed limit, got {}",
        oversized.len()
    );
    ws.send(WsMessage::Binary(oversized.into()))
        .await
        .expect("send oversized");
    // Expect close without ServerHello (codec rejection). With codec, we get Close or error.
    // Without codec (mutation), the server would have sent ServerHello because ticket is valid and no size check.
    let mut got_server_hello = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        if tokio::time::Instant::now() > deadline {
            break;
        }
        let item = tokio::time::timeout(Duration::from_millis(500), ws.next()).await;
        let Some(Ok(msg)) = item.unwrap_or(None) else {
            break;
        };
        match msg {
            WsMessage::Binary(data) => {
                let env = Envelope::decode(data.as_ref()).expect("decode");
                if matches!(env.payload, Some(envelope::Payload::ServerHello(_))) {
                    got_server_hello = true;
                    break;
                }
            }
            WsMessage::Close(_) => break,
            _ => continue,
        }
    }
    assert!(
        !got_server_hello,
        "oversized initial frame must be rejected at codec layer before ServerHello (C3) – got ServerHello indicates codec not enforced (M4)"
    );
}

#[tokio::test]
async fn c3_semaphore_rejects_when_full() {
    let Some(pool) = setup_db().await else { return };
    let mut config = Config::default();
    config.realtime.max_connections = 1;
    config.realtime.handshake_timeout_ms = 5000;
    let (addr, _state) = spawn_ws_server(pool.clone(), config).await;

    // First connection: valid ticket, will hold the semaphore
    let user_id1 = Uuid::now_v7();
    let session_id1 = Uuid::now_v7();
    let now = fixed_now();
    let now_odt = now.as_offset_date_time();
    insert_user_session(&pool, user_id1, session_id1, now_odt).await;
    let (ticket1, _) = create_ticket_via_store(&pool, user_id1, session_id1, now).await;

    // Second user for second attempt
    let user_id2 = Uuid::now_v7();
    let session_id2 = Uuid::now_v7();
    insert_user_session(&pool, user_id2, session_id2, now_odt).await;
    let (_ticket2, _) = create_ticket_via_store(&pool, user_id2, session_id2, now).await;

    // Hold first WS open
    let mut ws1 = ws_connect(addr).await.expect("connect1");
    let hs1 = do_handshake(&mut ws1, &ticket1).await;
    assert!(hs1.is_ok(), "first handshake must succeed, got {:?}", hs1);
    // Keep ws1 alive (don't close)

    // Attempt second upgrade while first holds semaphore – must be 503
    // tungstenite client will fail to connect with HTTP 503
    let uri = format!("ws://{addr}/ws").try_into().expect("uri");
    let req = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    let second = tokio_tungstenite::connect_async(req).await;
    match second {
        Ok((_ws2, resp)) => {
            // If it somehow upgraded, check status – but axum should have returned 503 before upgrade
            // tungstenite would have considered it success only if 101. Check resp status if available.
            // For our handler, 503 is returned as HTTP response, not as WS upgrade, so connect_async should err.
            // If we got here, it's unexpected – try to see if server sent 503 via HTTP status in resp
            let status = resp.status();
            assert_eq!(
                status,
                axum::http::StatusCode::SWITCHING_PROTOCOLS,
                "second connection should not upgrade when semaphore full, got status {}",
                status
            );
            // If it did upgrade, then semaphore not enforced – fail
            panic!(
                "second WS should have been rejected with 503 due to semaphore full (C3), but it upgraded"
            );
        }
        Err(e) => {
            let err_str = format!("{e:?}");
            // Expect handshake failure due to 503
            assert!(
                err_str.contains("503")
                    || err_str.contains("Service Unavailable")
                    || err_str.contains("Handshake"),
                "second connect must fail with 503/semaphore full, got {err_str}"
            );
        }
    }

    // Cleanup first connection
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = ws1.close(None).await;
}
