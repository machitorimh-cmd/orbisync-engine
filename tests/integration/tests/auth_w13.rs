//! W-13 authentication E2E: login, ticket issuance, audience isolation, expiry (D-7 supplement).
//!
//! Verifies the HTTP endpoints `POST /v1/auth/login` and
//! `POST /v1/realtime/tickets`, and that the resulting tickets are
//! required for `GET /ws` when `allow_stub_ticket = false`.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, dead_code)]

mod common;

use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use futures_util::{SinkExt, StreamExt};
use http_body_util::BodyExt as _;
use orbisync_application::{IdentityRepository, SecretString};
use orbisync_domain::{
    Clock, InstanceId, LoginId, Timestamp, Transform, User, World, WorldId, WorldInstance,
};
use orbisync_identity::{PasswordPolicy, PasswordService, token::AccessTokenService};
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::{AccessTokenTicketVerifier, HmacRealtimeTicketVerifier};
use orbisync_server::{
    delivery::DeliveryRegistry,
    realtime_ws::{RealtimeState, realtime_ws_handler},
};
use orbisync_testkit::{
    AllowAllAuthorizer, FakeIdentityStore, FakeRealtimeTicketStore, FakeWorldDirectoryStore,
    FixedClock,
};
use orbisync_transport_http::{HttpState, router};
use orbisync_world_runtime::RuntimeRegistry;
use prost::Message as ProstMessage;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tower::ServiceExt as _;

struct NopRefreshCreationStore;

#[async_trait::async_trait]
impl orbisync_application::RefreshTokenCreationStore for NopRefreshCreationStore {
    async fn create(
        &self,
        _command: orbisync_application::CreateRefreshTokenCommand,
    ) -> Result<(), orbisync_application::IdentityPortError> {
        Ok(())
    }
}

fn test_refresh_creation_store() -> Arc<dyn orbisync_application::RefreshTokenCreationStore> {
    Arc::new(NopRefreshCreationStore)
}

// ---------------------------------------------------------------------------
// constants and helpers
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
        .expect("test token service"),
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

fn test_realtime_hmac_key() -> Vec<u8> {
    b"test-realtime-ticket-hmac-key-32b!!".to_vec()
}

async fn create_alice_repo() -> (Arc<FakeIdentityStore>, Arc<PasswordService>) {
    let passwords =
        Arc::new(PasswordService::new(PasswordPolicy::new(Vec::new())).expect("password service"));
    let repo = Arc::new(FakeIdentityStore::new());
    let hash = passwords
        .hash(SecretString::new("Cedar!Lake7-Comet"))
        .await
        .expect("hash");
    let user_id = orbisync_domain::UserId::generate();
    let login_id = LoginId::new("alice").expect("login");
    let user = User::new(user_id, login_id, "Alice", now_ts()).expect("user");
    let credential = orbisync_domain::Credential::new(user_id, hash, now_ts());
    let account = orbisync_application::LoginAccount { user, credential };
    repo.insert_account(account);
    (repo, passwords)
}

fn test_login_service(
    repo: Arc<FakeIdentityStore>,
    passwords: Arc<PasswordService>,
    tokens: Arc<AccessTokenService>,
    clock: Arc<FixedClock>,
) -> Arc<orbisync_identity::LoginService> {
    use orbisync_application::LoginTransactionStore;
    let tx: Arc<dyn LoginTransactionStore> = repo.clone() as Arc<dyn LoginTransactionStore>;
    let repo_dyn: Arc<dyn IdentityRepository> = repo.clone() as Arc<dyn IdentityRepository>;
    Arc::new(orbisync_identity::LoginService::new(
        repo_dyn,
        tx,
        (*passwords).clone(),
        tokens,
        Arc::clone(&clock) as Arc<dyn Clock>,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
        900,
        2_592_000,
    ))
}

fn make_world() -> World {
    World::new(
        WorldId::generate(),
        "w13-world",
        None,
        Transform::identity(),
        100,
        now_ts(),
    )
    .expect("world")
}

fn make_instance(world_id: WorldId) -> WorldInstance {
    WorldInstance::new(InstanceId::generate(), world_id, 100, now_ts()).expect("instance")
}

fn client_hello_bytes(ticket: &str) -> Vec<u8> {
    let hello = orbisync_protocol::v1::ClientHello {
        supported_minor_min: 0,
        supported_minor_max: 0,
        realtime_ticket: ticket.to_owned(),
        client_name: "w13-test".to_owned(),
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
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
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
    let mut buf = Vec::new();
    env.encode(&mut buf).expect("encode");
    buf
}

async fn http_login(
    state: HttpState,
    login_id: &str,
    password: &str,
) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let body = serde_json::json!({ "login_id": login_id, "password": password });
    let req = Request::builder()
        .uri("/v1/auth/login")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn http_ticket(state: HttpState, bearer: Option<&str>) -> (StatusCode, serde_json::Value) {
    let app = router(state);
    let mut builder = Request::builder()
        .uri("/v1/realtime/tickets")
        .method("POST");
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    (status, json)
}

async fn ws_handshake_and_join(
    addr: std::net::SocketAddr,
    ticket: &str,
    instance_id: InstanceId,
) -> Result<(Envelope, Envelope), String> {
    let uri = format!("ws://{addr}/ws").try_into().expect("uri");
    let req = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("connect failed: {e}"))?;

    ws.send(Message::Binary(client_hello_bytes(ticket).into()))
        .await
        .map_err(|e| format!("send hello: {e}"))?;
    // Expect ServerHello
    let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .map_err(|_| "timeout waiting ServerHello".to_string())?
        .ok_or("ws closed after hello".to_string())?
        .map_err(|e| format!("ws recv: {e}"))?;
    if let Message::Binary(data) = msg {
        let env =
            Envelope::decode(data.as_ref()).map_err(|e| format!("decode ServerHello: {e}"))?;
        match env.payload {
            Some(envelope::Payload::ServerHello(_)) => {}
            Some(envelope::Payload::Error(e)) => {
                if e.code == "AUTHENTICATION_REQUIRED" {
                    return Err(format!("AUTHENTICATION_REQUIRED: {}", e.message));
                }
                return Err(format!(
                    "unexpected error after hello: {} {}",
                    e.code, e.message
                ));
            }
            other => return Err(format!("expected ServerHello, got {other:?}")),
        }
    } else {
        return Err("expected binary ServerHello".to_string());
    }

    // Join
    ws.send(Message::Binary(join_instance_bytes(instance_id).into()))
        .await
        .map_err(|e| format!("send join: {e}"))?;
    // Expect JoinAccepted or Error
    // Use helper to skip heartbeats
    let mut first: Option<Envelope> = None;
    let mut second: Option<Envelope> = None;
    for _ in 0..4 {
        let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
            .await
            .map_err(|_| "timeout waiting join response".to_string())?
            .ok_or("ws closed during join".to_string())?
            .map_err(|e| format!("ws recv join: {e}"))?;
        if let Message::Binary(data) = msg {
            let env = Envelope::decode(data.as_ref()).map_err(|e| format!("decode join: {e}"))?;
            match &env.payload {
                Some(envelope::Payload::Heartbeat(_))
                | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                Some(envelope::Payload::JoinAccepted(_))
                | Some(envelope::Payload::Snapshot(_))
                | Some(envelope::Payload::Error(_)) => {
                    if first.is_none() {
                        first = Some(env);
                    } else if second.is_none() {
                        second = Some(env);
                        break;
                    }
                }
                _ => continue,
            }
        }
    }
    let first = first.ok_or("missing first join response".to_string())?;
    let second = second.ok_or("missing second join response".to_string())?;
    // Check for error
    for env in [&first, &second] {
        if let Some(envelope::Payload::Error(e)) = &env.payload {
            return Err(format!("join error {}: {}", e.code, e.message));
        }
    }
    // Must have JoinAccepted + Snapshot
    let has_accepted = matches!(first.payload, Some(envelope::Payload::JoinAccepted(_)))
        || matches!(second.payload, Some(envelope::Payload::JoinAccepted(_)));
    let has_snapshot = matches!(first.payload, Some(envelope::Payload::Snapshot(_)))
        || matches!(second.payload, Some(envelope::Payload::Snapshot(_)));
    if !has_accepted || !has_snapshot {
        return Err(format!(
            "expected JoinAccepted+Snapshot, got {first:?} {second:?}"
        ));
    }
    // Reason: test discards must_use value intentionally; the value is not needed for the assertion and dropping is explicit (A-3 allow with reason).
    #[allow(clippy::let_underscore_must_use)]
    let _ = ws.close(None).await;
    Ok((first, second))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn w13_login_ticket_ws_join_with_real_verifier_succeeds() {
    // Setup identity
    let (repo, passwords) = create_alice_repo().await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let world = make_world();
    let instance = make_instance(world.id());
    let instance_id = instance.id();

    // HTTP state for login/ticket (C3: must wire RealtimeTicketStore + hmac, no JWT fallback)
    let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
        Arc::new(FakeRealtimeTicketStore::default());
    let http_state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::clone(&ticket_store));

    // 1. login
    let (status, json) = http_login(http_state.clone(), "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(status, StatusCode::OK, "login must succeed, got {json}");
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .expect("access_token")
        .to_owned();
    assert!(!access_token.is_empty());

    // 2. ticket
    let (status, json) = http_ticket(http_state, Some(&access_token)).await;
    assert_eq!(status, StatusCode::OK, "ticket must succeed, got {json}");
    let ticket = json
        .get("realtime_ticket")
        .and_then(|v| v.as_str())
        .expect("realtime_ticket")
        .to_owned();
    assert!(!ticket.is_empty());

    // 3. WS join with real verifier (allow_stub_ticket = false)
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = orbisync_interest::UniformGrid::default();
    let verifier = Arc::new(HmacRealtimeTicketVerifier::new(
        Arc::clone(&ticket_store) as Arc<dyn orbisync_application::RealtimeTicketStore>,
        test_realtime_hmac_key(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .with_world_store(store as Arc<dyn orbisync_application::WorldDirectoryStore>)
        .with_tickets(verifier as Arc<dyn orbisync_realtime::gateway::RealtimeTicketVerifier>)
        .with_checkpoint_store(
            Arc::new(common::NoopCheckpointStore) as Arc<dyn orbisync_application::CheckpointStore>
        )
        .with_world_authorizer(Arc::new(AllowAllAuthorizer))
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

    let result = ws_handshake_and_join(addr, &ticket, instance_id).await;
    assert!(
        result.is_ok(),
        "ws join with real ticket must succeed: {:?}",
        result.err()
    );
}

#[tokio::test]
async fn w13_wrong_password_and_missing_account_both_401_same() {
    let (repo, passwords) = create_alice_repo().await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>);

    let (status_wrong, json_wrong) = http_login(state.clone(), "alice", "Wrong!Lake7-Comet").await;
    assert_eq!(
        status_wrong,
        StatusCode::UNAUTHORIZED,
        "wrong password must be 401, got {json_wrong}"
    );
    let code_wrong = json_wrong
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code_wrong, "AUTHENTICATION_REQUIRED");

    let (status_missing, json_missing) = http_login(state, "nobody", "Wrong!Lake7-Comet").await;
    assert_eq!(
        status_missing,
        StatusCode::UNAUTHORIZED,
        "missing account must be 401, got {json_missing}"
    );
    let code_missing = json_missing
        .get("error")
        .and_then(|e| e.get("code"))
        .and_then(|c| c.as_str())
        .unwrap_or("");
    assert_eq!(code_missing, "AUTHENTICATION_REQUIRED");

    // Ensure bodies are indistinguishable (same code, same message shape)
    assert_eq!(
        code_wrong, code_missing,
        "wrong password and missing account must map to same 401 code to prevent enumeration"
    );
}

#[tokio::test]
async fn w13_expired_ticket_is_rejected() {
    let (repo, passwords) = create_alice_repo().await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    // issue ticket at fixed time (via login flow, not direct)
    let _user_id = {
        let account = repo
            .find_login(&LoginId::new("alice").expect("login"))
            .await
            .expect("find")
            .expect("present");
        account.user.id()
    };
    // Need session id from a valid access token: login to get one
    let ticket_store: Arc<dyn orbisync_application::RealtimeTicketStore> =
        Arc::new(FakeRealtimeTicketStore::default());
    let http_state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::clone(&ticket_store));
    let (status, json) = http_login(http_state.clone(), "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(status, StatusCode::OK);
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let (status, json) = http_ticket(http_state, Some(&access_token)).await;
    assert_eq!(status, StatusCode::OK);
    let ticket = json
        .get("realtime_ticket")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Advance clock beyond ticket TTL + skew (60 + 30 = 90, so 91)
    clock.advance_millis(91_000);

    // Try WS with expired ticket  — Emust be AUTHENTICATION_REQUIRED
    let world = make_world();
    let instance = make_instance(world.id());
    let _instance_id = instance.id();
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let verifier = Arc::new(HmacRealtimeTicketVerifier::new(
        Arc::clone(&ticket_store) as Arc<dyn orbisync_application::RealtimeTicketStore>,
        test_realtime_hmac_key(),
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .with_world_store(store as Arc<dyn orbisync_application::WorldDirectoryStore>)
        .with_tickets(verifier as Arc<dyn orbisync_realtime::gateway::RealtimeTicketVerifier>)
        .with_world_authorizer(Arc::new(AllowAllAuthorizer))
        .build(),
    );
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Expired ticket is rejected at the handshake stage (after ServerHello, before Join).
    // The server sends ServerHello then AUTHENTICATION_REQUIRED and closes.
    let uri = format!("ws://{addr}/ws").try_into().expect("uri");
    let req = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws connect");
    ws.send(Message::Binary(client_hello_bytes(&ticket).into()))
        .await
        .expect("send hello");
    // RV-A C1: verification is now before ServerHello, so expired ticket yields
    // AUTHENTICATION_REQUIRED directly (no ServerHello). Keep loop tolerant of
    // both old (ServerHello then Error) and new (Error only) order.
    let mut got_hello = false;
    let mut got_auth_err = false;
    for _ in 0..4 {
        let msg = match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => panic!(
                "ws stream error while waiting for AUTHENTICATION_REQUIRED (expired ticket): {e}; got_hello={got_hello}, got_auth_err={got_auth_err}"
            ),
            Ok(None) => panic!(
                "ws closed while waiting for AUTHENTICATION_REQUIRED (expired ticket): got_hello={got_hello}, got_auth_err={got_auth_err}"
            ),
            Err(_) => panic!(
                "timeout waiting for AUTHENTICATION_REQUIRED (expired ticket) after 3s: got_hello={got_hello}, got_auth_err={got_auth_err}; expected Error(AUTHENTICATION_REQUIRED) but no such message arrived  — Eticket expiry verification may be disabled or verifier bypassed"
            ),
        };
        if let Message::Binary(data) = msg {
            let env = Envelope::decode(data.as_ref()).expect("decode");
            match env.payload {
                Some(envelope::Payload::ServerHello(_)) => got_hello = true,
                Some(envelope::Payload::Error(e)) if e.code == "AUTHENTICATION_REQUIRED" => {
                    got_auth_err = true;
                    break;
                }
                Some(envelope::Payload::Heartbeat(_))
                | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                other => panic!(
                    "expected AUTHENTICATION_REQUIRED, got {other:?} (got_hello={got_hello})"
                ),
            }
        }
    }
    assert!(
        got_auth_err,
        "expired ticket must be rejected with AUTHENTICATION_REQUIRED; got_hello={got_hello}, got_auth_err={got_auth_err} (expected Error code AUTHENTICATION_REQUIRED but it was not received; ticket expiry verification may be disabled or verifier bypassed)"
    );

    // Mutation check: if ticket verification were bypassed (stub), this test would go green (incorrectly succeed).
    // Documented here; manual mutation is to replace AccessTokenTicketVerifier with StubTicketVerifier and observe green.
}

#[tokio::test]
async fn w13_access_token_as_ws_ticket_is_rejected() {
    let (repo, passwords) = create_alice_repo().await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    let http_state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>);
    let (status, json) = http_login(http_state, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(status, StatusCode::OK);
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Try to use access token directly as realtime ticket
    let world = make_world();
    let instance = make_instance(world.id());
    let _instance_id = instance.id();
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let verifier = Arc::new(AccessTokenTicketVerifier::new(
        Arc::clone(&tokens),
        Arc::clone(&clock) as Arc<dyn Clock>,
    ));
    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn Clock>,
        )
        .with_world_store(store as Arc<dyn orbisync_application::WorldDirectoryStore>)
        .with_tickets(verifier as Arc<dyn orbisync_realtime::gateway::RealtimeTicketVerifier>)
        .with_world_authorizer(Arc::new(AllowAllAuthorizer))
        .build(),
    );
    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Access token must be rejected as ws ticket (audience isolation). Expect ServerHello then AUTHENTICATION_REQUIRED.
    let uri = format!("ws://{addr}/ws").try_into().expect("uri");
    let req = tokio_tungstenite::tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    let (mut ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .expect("ws connect");
    ws.send(Message::Binary(client_hello_bytes(&access_token).into()))
        .await
        .expect("send hello");
    // RV-A C1: verification is now before ServerHello, so invalid audience yields
    // AUTHENTICATION_REQUIRED directly (no ServerHello). Tolerant of both orders.
    let mut got_hello = false;
    let mut got_auth_err = false;
    for _ in 0..4 {
        let msg = match tokio::time::timeout(Duration::from_secs(3), ws.next()).await {
            Ok(Some(Ok(msg))) => msg,
            Ok(Some(Err(e))) => panic!(
                "ws stream error while waiting for AUTHENTICATION_REQUIRED (audience isolation): {e}; got_hello={got_hello}, got_auth_err={got_auth_err}"
            ),
            Ok(None) => panic!(
                "ws closed while waiting for AUTHENTICATION_REQUIRED (audience isolation): got_hello={got_hello}, got_auth_err={got_auth_err}"
            ),
            Err(_) => panic!(
                "timeout waiting for AUTHENTICATION_REQUIRED (audience isolation) after 3s: got_hello={got_hello}, got_auth_err={got_auth_err}; expected Error(AUTHENTICATION_REQUIRED) but no such message arrived  — Eaudience isolation may be broken (issue_realtime_ticket using same audience as access token)"
            ),
        };
        if let Message::Binary(data) = msg {
            let env = Envelope::decode(data.as_ref()).expect("decode");
            match env.payload {
                Some(envelope::Payload::ServerHello(_)) => got_hello = true,
                Some(envelope::Payload::Error(e)) if e.code == "AUTHENTICATION_REQUIRED" => {
                    got_auth_err = true;
                    break;
                }
                Some(envelope::Payload::Heartbeat(_))
                | Some(envelope::Payload::HeartbeatAck(_)) => continue,
                other => panic!(
                    "expected AUTHENTICATION_REQUIRED, got {other:?} (got_hello={got_hello})"
                ),
            }
        }
    }
    assert!(
        got_auth_err,
        "access token as ws ticket must be rejected with AUTHENTICATION_REQUIRED (audience isolation); got_hello={got_hello}, got_auth_err={got_auth_err} (expected Error code AUTHENTICATION_REQUIRED but it was not received; audience isolation may be broken)"
    );

    // Mutation: if issue_realtime_ticket used same audience as access token, this test would go red (incorrectly succeed).
}

#[tokio::test]
async fn w13_realtime_ticket_as_bearer_for_worlds_is_401() {
    let (repo, passwords) = create_alice_repo().await;
    let clock = fixed_clock();
    let tokens = test_token_service();
    // login and get ticket
    let http_state_login = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>);
    let (status, json) = http_login(http_state_login, "alice", "Cedar!Lake7-Comet").await;
    assert_eq!(status, StatusCode::OK);
    let access_token = json
        .get("access_token")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();
    let http_state_ticket = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>);
    let (status, json) = http_ticket(http_state_ticket, Some(&access_token)).await;
    assert_eq!(status, StatusCode::OK);
    let ticket = json
        .get("realtime_ticket")
        .and_then(|v| v.as_str())
        .unwrap()
        .to_owned();

    // Try to use ticket as Bearer for POST /v1/worlds  — Emust be 401
    // Use real HttpState with world directory and token service
    let http_state = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>)
    .with_world_directory(Arc::new(orbisync_application::WorldDirectoryUseCase::new(
        FakeWorldDirectoryStore::new(),
        AllowAllAuthorizer,
    )))
    .with_allow_stub_bearer(false);

    // Use the ticket as bearer
    let app = router(http_state);
    let body = serde_json::json!({ "name": "test-world", "capacity": 100 });
    let req = Request::builder()
        .uri("/v1/worlds")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {ticket}"))
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "realtime ticket must not be accepted as Bearer for REST (audience isolation)"
    );

    // Also sanity: access token should succeed (create world)
    let http_state2 = HttpState::new(
        vec![],
        "orbisync",
        "0.1.0",
        PROTOCOL_MAJOR,
        Arc::clone(&clock) as Arc<dyn Clock>,
        900,
        2_592_000,
        b"test-hmac-key-for-unit-tests-32b!!".to_vec(),
    )
    .with_token_service(Arc::clone(&tokens))
    .with_identity_repository(Arc::clone(&repo) as Arc<dyn IdentityRepository>)
    .with_password_service(Arc::clone(&passwords))
    .with_login_service(test_login_service(
        Arc::clone(&repo),
        Arc::clone(&passwords),
        Arc::clone(&tokens),
        Arc::clone(&clock),
    ))
    .with_refresh_creation_store(test_refresh_creation_store())
    .with_realtime_ticket_hmac_key(test_realtime_hmac_key())
    .with_realtime_ticket_store(Arc::new(FakeRealtimeTicketStore::default())
        as Arc<dyn orbisync_application::RealtimeTicketStore>)
    .with_world_directory(Arc::new(orbisync_application::WorldDirectoryUseCase::new(
        FakeWorldDirectoryStore::new(),
        AllowAllAuthorizer,
    )))
    .with_allow_stub_bearer(false);
    let app2 = router(http_state2);
    let body2 = serde_json::json!({ "name": "test-world-2", "capacity": 100 });
    let req2 = Request::builder()
        .uri("/v1/worlds")
        .method("POST")
        .header("content-type", "application/json")
        .header("authorization", format!("Bearer {access_token}"))
        .body(Body::from(body2.to_string()))
        .unwrap();
    let resp2 = app2.oneshot(req2).await.unwrap();
    // This may succeed or fail due to world store being empty but should not be 401
    assert_ne!(
        resp2.status(),
        StatusCode::UNAUTHORIZED,
        "access token should be accepted for /v1/worlds, not 401"
    );
}
