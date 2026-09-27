//! Real-WebSocket E2E wiring for the pre-commit extension validation hook
//! (ADR-025, improvement 2.2 in
//! `docs/design/generalization-and-llm-app-platform.md`).
//!
//! These tests drive the real `realtime_ws_handler` over a real TCP/WebSocket
//! connection (same harness as `entity_w16.rs`), so they exercise the actual
//! `realtime_ws_connection_runtime.rs` dispatch path — not the actor
//! directly. The pre-commit gate and registration store are fakes at this
//! layer: the real HTTP transport, signing and SSRF-egress-policy contract
//! are already exercised against a real local HTTPS listener in
//! `crates/orbisync-extensions/src/precommit.rs`'s own tests
//! (`real_loopback_transport_*`, `loopback_is_rejected_by_default_ssrf_policy`).
//! This file's job is to verify the wiring: insertion point, opt-out
//! behavior, deny/allow effect on the actual command outcome, and that the
//! 20Hz transform stream never reaches the hook.

#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic, dead_code)]

mod common;

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use orbisync_application::{
    ApplicationError, ApplicationErrorKind, CheckpointStore, CreateRealtimeTicketCommand,
    IdentityRepository, RealtimeTicketStore, WorldDirectoryStore,
};
use orbisync_domain::{InstanceId, Timestamp, Transform, World, WorldId, WorldInstance};
use orbisync_extensions::{
    DeliveryError, DnsResolver, ExtensionRegistration, ExtensionRegistrationStore, ExtensionStatus,
    PreCommitDecision, PreCommitGate, PreCommitOperation, PreCommitValidationGate,
    PreCommitValidationPolicy, PreCommitValidationRequest, ReqwestPreCommitClient, SecretProvider,
};
use orbisync_interest::UniformGrid;
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_protocol::{PROTOCOL_MAJOR, WEBSOCKET_SUBPROTOCOL};
use orbisync_realtime::gateway::{HmacRealtimeTicketVerifier, StubTicketVerifier};
use orbisync_server::{
    delivery::DeliveryRegistry,
    realtime_ws::{RealtimeState, realtime_ws_handler},
};
use orbisync_testkit::{
    AllowEntityOwnerAuthorizer, FakeIdentityRepository, FakeRealtimeTicketStore,
    FakeWorldDirectoryStore, FixedClock,
};
use orbisync_world_runtime::RuntimeRegistry;
use orbisync_world_runtime::command::InstanceCommand;
use prost::Message as ProstMessage;
use prost_types::{Struct, Value, value::Kind};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

const RECV_TIMEOUT: Duration = Duration::from_secs(3);
const TICKET: &str = "stub-ticket-precommit";

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
        "precommit-e2e-world",
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
        client_name: "precommit-e2e".to_owned(),
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

fn number_value(n: f64) -> Value {
    Value {
        kind: Some(Kind::NumberValue(n)),
    }
}

fn spawn_args_global(pos: orbisync_domain::Vec3) -> Struct {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("kind".to_owned(), string_value("object"));
    fields.insert("visibility".to_owned(), string_value("global"));
    fields.insert("position_x".to_owned(), number_value(f64::from(pos.x())));
    fields.insert("position_y".to_owned(), number_value(f64::from(pos.y())));
    fields.insert("position_z".to_owned(), number_value(f64::from(pos.z())));
    Struct { fields }
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
        instance_revision: None,
        operation: operation.to_owned(),
        arguments,
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

fn update_args(component_key: &str, value: i64) -> Struct {
    let mut fields = std::collections::BTreeMap::new();
    fields.insert("component_key".to_owned(), string_value(component_key));
    fields.insert("value".to_owned(), number_value(value as f64));
    Struct { fields }
}

fn transform_input_bytes(entity_id: orbisync_domain::EntityId, sequence: u64) -> Vec<u8> {
    transform_input_bytes_with_revision(entity_id, 0, sequence)
}

fn transform_input_bytes_with_revision(
    entity_id: orbisync_domain::EntityId,
    expected_revision: u64,
    sequence: u64,
) -> Vec<u8> {
    let input = orbisync_protocol::v1::TransformInput {
        entity_id: entity_id.to_string(),
        transform: Some(orbisync_protocol::v1::Transform {
            position_x: 0.0,
            position_y: 0.0,
            position_z: 0.0,
            rotation_x: 0.0,
            rotation_y: 0.0,
            rotation_z: 0.0,
            rotation_w: 1.0,
        }),
        expected_revision,
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

/// Always returns a fixed decision, and counts how many times it was called
/// — used to assert `UpdateTransform` never reaches the hook (§2.3) and that
/// duplicate/replayed commands do not multiply calls beyond the dedup
/// leader.
struct ScriptedGate {
    calls: AtomicUsize,
    decision: std::sync::Mutex<PreCommitDecision>,
}

impl ScriptedGate {
    fn new(decision: PreCommitDecision) -> Self {
        Self {
            calls: AtomicUsize::new(0),
            decision: std::sync::Mutex::new(decision),
        }
    }
}

#[async_trait::async_trait]
impl PreCommitGate for ScriptedGate {
    async fn validate(
        &self,
        _registration: &ExtensionRegistration,
        _request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.decision.lock().expect("lock").clone()
    }
}

/// Denies requests for one specific [`PreCommitOperation`] and allows every
/// other operation. Used to prove a deny is scoped to the operation it
/// actually denies (e.g. a denied `delete` must not also block a
/// subsequent `update` on the same entity — that would make a
/// "still exists" assertion vacuous).
struct DenyOperationGate {
    denied_operation: PreCommitOperation,
    reason: String,
    calls: AtomicUsize,
}

impl DenyOperationGate {
    fn new(denied_operation: PreCommitOperation, reason: impl Into<String>) -> Self {
        Self {
            denied_operation,
            reason: reason.into(),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl PreCommitGate for DenyOperationGate {
    async fn validate(
        &self,
        _registration: &ExtensionRegistration,
        request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if request.operation == self.denied_operation {
            PreCommitDecision::Deny {
                reason: self.reason.clone(),
            }
        } else {
            PreCommitDecision::Allow
        }
    }
}

/// Allows every request, but sleeps `delay` before responding when the
/// request's `component_key` equals `slow_component_key`. Used to model a
/// slow external judgment on one specific command while other commands
/// (different `component_key`, different entity, different instance) get an
/// immediate response — the same asymmetry a real network round trip would
/// have.
struct SelectiveDelayGate {
    slow_component_key: String,
    delay: Duration,
    calls: AtomicUsize,
}

impl SelectiveDelayGate {
    fn new(slow_component_key: impl Into<String>, delay: Duration) -> Self {
        Self {
            slow_component_key: slow_component_key.into(),
            delay,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl PreCommitGate for SelectiveDelayGate {
    async fn validate(
        &self,
        _registration: &ExtensionRegistration,
        request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if request.component_key.as_deref() == Some(self.slow_component_key.as_str()) {
            tokio::time::sleep(self.delay).await;
        }
        PreCommitDecision::Allow
    }
}

/// Signals (via `started`) the instant its `validate` call begins, then
/// sleeps `delay` before answering Allow. Used to trigger an external event
/// (here, revoking a permission) at a point provably *after* the hook call
/// has started but *before* it resolves — proving a fix must re-check
/// permissions after the hook's wait, not only immediately before it.
struct SignalingDelayGate {
    started: Arc<AtomicBool>,
    delay: Duration,
    calls: AtomicUsize,
}

impl SignalingDelayGate {
    fn new(started: Arc<AtomicBool>, delay: Duration) -> Self {
        Self {
            started,
            delay,
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl PreCommitGate for SignalingDelayGate {
    async fn validate(
        &self,
        _registration: &ExtensionRegistration,
        _request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.store(true, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        PreCommitDecision::Allow
    }
}

/// `WorldAuthorizer` test double that grants `entity.spawn` /
/// `entity.update.own` until `revoked` is flipped, and counts every
/// `require` call — used to prove (not merely assert) whether anything on
/// the pre-commit hook path asks the authorizer again after the initial
/// handshake resolved `WorldPermissions`.
struct RevocableAuthorizer {
    revoked: AtomicBool,
    calls: AtomicUsize,
}

impl RevocableAuthorizer {
    fn new() -> Self {
        Self {
            revoked: AtomicBool::new(false),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl orbisync_application::WorldAuthorizer for RevocableAuthorizer {
    async fn require(
        &self,
        _actor: orbisync_domain::UserId,
        permission: &str,
    ) -> Result<(), ApplicationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.revoked.load(Ordering::SeqCst) {
            return Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "permission denied",
            ));
        }
        match permission {
            "entity.spawn" | "entity.update.own" => Ok(()),
            _ => Err(ApplicationError::new(
                ApplicationErrorKind::NotAuthorized,
                "permission denied",
            )),
        }
    }
}

/// Registration store that reports one Active registration subscribed to
/// every pre-commit capability. Mirrors "at most one Active extension per
/// capability" (ADR-025 §2.7).
struct SingleRegistrationStore {
    registration: ExtensionRegistration,
}

impl SingleRegistrationStore {
    fn new() -> Self {
        Self {
            registration: ExtensionRegistration {
                extension_id: uuid::Uuid::now_v7(),
                name: String::from("zone-guard"),
                description: None,
                endpoint: String::from("https://zone-guard.example/precommit"),
                subscribed_events: BTreeSet::new(),
                capabilities: BTreeSet::from([
                    String::from(PreCommitOperation::Spawn.capability()),
                    String::from(PreCommitOperation::Update.capability()),
                    String::from(PreCommitOperation::Delete.capability()),
                ]),
                token_scopes: BTreeSet::new(),
                status: ExtensionStatus::Active,
                signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_ZONE_GUARD"),
            },
        }
    }
}

#[async_trait::async_trait]
impl ExtensionRegistrationStore for SingleRegistrationStore {
    async fn save_registration(
        &self,
        _registration: ExtensionRegistration,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }

    async fn find_registration(
        &self,
        _extension_id: uuid::Uuid,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        Ok(Some(self.registration.clone()))
    }

    async fn find_active_registration_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        if self.registration.capabilities.contains(capability) {
            Ok(Some(self.registration.clone()))
        } else {
            Ok(None)
        }
    }
}

struct TestServer {
    addr: std::net::SocketAddr,
    gate_calls: Option<Arc<AtomicUsize>>,
}

/// Grants every permission to every actor — used only by
/// `stale_revision_during_pending_hook_wait_is_not_applied`, which needs two
/// different authenticated users to both be able to mutate the same entity
/// (to race a revision change against a pending hook call) independent of
/// Core's ownership rule, which is exercised separately by
/// `core_permission_denied_even_when_hook_allows`.
struct AllowAllWorldAuthorizer;

#[async_trait::async_trait]
impl orbisync_application::WorldAuthorizer for AllowAllWorldAuthorizer {
    async fn require(
        &self,
        _actor: orbisync_domain::UserId,
        _permission: &str,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }
}

async fn spawn_server(
    world: World,
    instance: WorldInstance,
    pre_commit: Option<(Arc<dyn PreCommitGate>, Arc<dyn ExtensionRegistrationStore>)>,
) -> TestServer {
    spawn_server_with_authorizer(
        world,
        instance,
        pre_commit,
        Arc::new(AllowEntityOwnerAuthorizer),
    )
    .await
}

async fn spawn_server_with_authorizer(
    world: World,
    instance: WorldInstance,
    pre_commit: Option<(Arc<dyn PreCommitGate>, Arc<dyn ExtensionRegistrationStore>)>,
    authorizer: Arc<dyn orbisync_application::WorldAuthorizer>,
) -> TestServer {
    let store = Arc::new(FakeWorldDirectoryStore::new());
    store.insert_world(world);
    store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(common::NoopCheckpointStore);
    let mut builder = RealtimeState::builder(
        orbisync_config::Config::default().realtime,
        Arc::clone(&registry),
        Arc::clone(&delivery),
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
    )
    .with_world_store(store.clone() as Arc<dyn WorldDirectoryStore>)
    .with_tickets(Arc::new(StubTicketVerifier::new()))
    .with_checkpoint_store(checkpoint_store)
    .with_world_authorizer(authorizer)
    .with_interest_grid(grid);
    if let Some((gate, registrations)) = pre_commit {
        builder = builder
            .with_pre_commit_gate(gate)
            .with_extension_registrations(registrations);
    }
    let state = Arc::new(builder.build());
    // Mirrors main.rs's initial refresh before serving connections: makes
    // `state.spawn_hook_active` reflect whatever `registrations` (if any)
    // actually reports for `hooks:entity:spawn`, through the real lookup
    // path, rather than requiring every test to inject the flag by hand.
    // A no-op when no registration store is configured.
    state.refresh_spawn_hook_active_cache().await;
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
        gate_calls: None,
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
            Ok(Some(Err(e))) => panic!("ws stream error: {e}"),
            Ok(None) => return None,
            Err(_) => panic!("timeout waiting for envelope after {RECV_TIMEOUT:?}"),
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
    let response = recv_envelope(ws).await.expect("hello response");
    assert!(
        matches!(response.payload, Some(envelope::Payload::ServerHello(_))),
        "expected ServerHello, got {:?}",
        response.payload
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
            || (is_accepted(&second) && is_snapshot(&first)),
        "expected JoinAccepted + Snapshot, got {:?} / {:?}",
        first.payload,
        second.payload
    );
}

async fn expect_entity_applied<S>(ws: &mut S, entity: orbisync_domain::EntityId)
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let env = recv_envelope(ws)
                .await
                .unwrap_or_else(|| panic!("ws closed waiting for EntityCommand for {entity}"));
            if let Some(envelope::Payload::EntityCommand(cmd)) = &env.payload
                && cmd.entity_id == entity.to_string()
            {
                return;
            }
        }
    })
    .await
    .expect("EntityCommand broadcast within timeout");
}

async fn expect_error<S>(ws: &mut S) -> orbisync_protocol::v1::ErrorMessage
where
    S: StreamExt<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    tokio::time::timeout(RECV_TIMEOUT, async {
        loop {
            let env = recv_envelope(ws)
                .await
                .unwrap_or_else(|| panic!("ws closed while waiting for Error"));
            if let Some(envelope::Payload::Error(e)) = &env.payload {
                return e.clone();
            }
        }
    })
    .await
    .expect("Error within timeout")
}

/// T1: with no pre-commit gate configured (the default), a spawn command
/// behaves exactly as it did before ADR-025 — no capability lookup, no HTTP
/// call, command applied.
#[tokio::test]
async fn opt_out_without_a_configured_gate_behaves_unchanged() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, None).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let bytes = entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// T2: a configured gate that allows lets the spawn proceed and the gate is
/// actually consulted (call count is 1).
#[tokio::test]
async fn allow_decision_lets_the_command_proceed() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let gate_calls = Arc::new(AtomicUsize::new(0));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;
    let _ = &server.gate_calls;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let bytes = entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        1,
        "the gate must be consulted exactly once for the Leader admission of this command"
    );
    let _ = gate_calls;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// T3: a configured gate that denies rejects the command before it reaches
/// the actor — the client receives an Error and no EntityCommand broadcast
/// (state was not changed).
#[tokio::test]
async fn deny_decision_rejects_the_command_before_it_is_applied() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Deny {
        reason: String::from("inside no-fly zone"),
    }));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let bytes = entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send spawn");

    let error = expect_error(&mut a).await;
    assert_eq!(error.code, "PRE_COMMIT_DENIED");
    assert_eq!(error.message, "inside no-fly zone");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// T11: `UpdateTransform` (the 20Hz position stream) never reaches the hook
/// itself, even when a gate is configured and would deny everything
/// (ADR-025 §2.3) — the HTTP round trip is never added to this path.
///
/// This no longer means auto-create is unguarded, though: independent
/// verification (worker-brief §8, priority item 2) found that the previous
/// behavior let a client bypass an active spawn hook entirely by never
/// sending `SpawnEntity` at all — sending a bare `UpdateTransform` for an
/// unknown `entity_id` auto-created it (M2) with no check at all. Since
/// `spawn_server` registers `SingleRegistrationStore`, which subscribes to
/// `hooks:entity:spawn`, `state.spawn_hook_active` is now true for this
/// test, and the actor rejects the auto-create outright
/// (`EXPLICIT_SPAWN_REQUIRED`) instead of creating an unapproved entity.
/// `gate.calls` staying at 0 still proves the hook's HTTP path itself was
/// never touched — the fix does not add synchronous I/O to this path, it
/// only refuses the auto-create using a plain in-process flag (ADR-025
/// "既知の迂回経路"). See `transform_auto_create_still_works_without_a_spawn_hook`
/// for the opt-out case, and
/// `existing_entity_transform_updates_still_work_when_a_spawn_hook_is_active`
/// for proof this does not affect ordinary movement of an entity that
/// already exists.
#[tokio::test]
async fn transform_auto_create_is_rejected_when_a_spawn_hook_is_active() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Deny {
        reason: String::from("would deny everything"),
    }));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let bytes = transform_input_bytes(entity, 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform");
    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "EXPLICIT_SPAWN_REQUIRED",
        "auto-create for an unknown entity must be refused while a spawn hook \
         is active, got {:?}",
        error.code
    );
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        0,
        "the rejection must come from the in-process spawn_hook_active flag, \
         not from an HTTP call to the hook — the hook is never consulted for \
         UpdateTransform"
    );
}

/// Opt-out compatibility (worker-brief §8): when no Active `hooks:entity:spawn`
/// registration exists, `UpdateTransform` auto-create behaves exactly as it
/// did before ADR-025 — this is the pre-existing M2 behavior every
/// deployment without a spawn hook must keep getting.
#[tokio::test]
async fn transform_auto_create_still_works_without_a_spawn_hook() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(world, instance, None).await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let bytes = transform_input_bytes(entity, 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform");
    let env = tokio::time::timeout(RECV_TIMEOUT, recv_envelope(&mut a))
        .await
        .expect("response within timeout")
        .expect("ws did not close");
    assert!(
        matches!(env.payload, Some(envelope::Payload::StateDelta(_))),
        "auto-create must still work with no spawn hook registered, got {:?}",
        env.payload
    );
}

/// The `spawn_hook_active` check only guards the auto-create branch (no
/// existing entity). An entity that already exists — created through an
/// explicit, hook-consulted `SpawnEntity` — must keep receiving ordinary
/// `UpdateTransform` movement exactly as before, proving the fix does not
/// widen into blocking normal gameplay.
#[tokio::test]
async fn existing_entity_transform_updates_still_work_when_a_spawn_hook_is_active() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    let bytes = transform_input_bytes_with_revision(entity, 1, 4);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform");
    let env = tokio::time::timeout(RECV_TIMEOUT, recv_envelope(&mut a))
        .await
        .expect("response within timeout")
        .expect("ws did not close");
    assert!(
        matches!(env.payload, Some(envelope::Payload::StateDelta(_))),
        "an ordinary transform update on an already-spawned entity must still \
         work while a spawn hook is active, got {:?}",
        env.payload
    );
}

/// Core permission checks are Core's exclusively: a hook that allows
/// everything must not let a non-owner mutate another user's entity
/// (ADR-025 §2.9 — the hook is an additional deny-only gate, never a way to
/// grant what Core would otherwise refuse).
#[tokio::test]
async fn core_permission_denied_even_when_hook_allows() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

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
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    // B (not the owner) tries to update A's entity. The hook allows
    // everything, so a rejection here can only come from Core's ownership
    // check, proving the hook cannot widen Core's authorization.
    b.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("test.key", 1)), 3).into(),
    ))
    .await
    .expect("send update");
    let error = expect_error(&mut b).await;
    assert!(
        error.code == "NOT_OWNER" || error.code == "INVALID_ARGUMENT",
        "non-owner update must be rejected by Core even though the hook allows, got {}",
        error.code
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

/// While one command's hook call is pending, a competing update on the same
/// entity from another connection completes and bumps the revision. When
/// the slow hook finally allows, the actor must reject the now-stale
/// `expected_revision` rather than applying an approval based on state that
/// no longer holds (ADR-025 §2.5: the hook never re-checks revision itself;
/// the actor's existing optimistic-concurrency check is the backstop that
/// makes this safe).
#[tokio::test]
async fn stale_revision_during_pending_hook_wait_is_not_applied() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(SelectiveDelayGate::new(
        "app.slow-trigger",
        Duration::from_millis(400),
    ));
    let server = spawn_server_with_authorizer(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
        Arc::new(AllowAllWorldAuthorizer),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    // A spawns the entity that both A and B will race to update.
    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;
    // Drain B's echo of A's spawn broadcast (B is also a member and receives
    // every broadcast on this instance) before relying on a positional read
    // from B below, or the leftover spawn echo would be mistaken for the
    // later update response.
    recv_envelope(&mut b).await.expect("b spawn echo");

    // A sends an update whose hook call will be held for 400ms. Do not wait
    // for A's response yet — it must stay in flight while B races ahead.
    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            1,
            "update",
            Some(update_args("app.slow-trigger", 1)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send slow update");
    // Give the server time to admit A's command and start the (delayed)
    // hook call before B's command is sent.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // B updates the same entity at the same expected_revision with a hook
    // call that is not delayed; it completes and bumps the revision to 2
    // while A's hook call is still sleeping.
    b.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("app.fast-key", 2)), 3).into(),
    ))
    .await
    .expect("send fast update");
    expect_entity_applied(&mut b, entity).await;

    // A's hook now returns Allow, but the actor's expected_revision (1) no
    // longer matches the current revision (2): the stale approval must not
    // be applied.
    let error = expect_error(&mut a).await;
    assert!(
        error.code.contains("REVISION") || error.code == "STALE_REVISION",
        "a stale hook approval must not bypass the actor's revision check, got {}",
        error.code
    );
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        3,
        "the hook must be reached by A's spawn, A's update, and B's update"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

/// While one connection's command is waiting on a slow hook response, an
/// unrelated command targeting a different entity on the same instance,
/// sent from a different connection, must not be blocked behind that wait.
/// This is the property the hook's insertion point (before the per-instance
/// `command_durability_guard`) exists to guarantee.
#[tokio::test]
async fn slow_hook_does_not_block_unrelated_operations_on_the_same_instance() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let delay = Duration::from_millis(500);
    let gate = Arc::new(SelectiveDelayGate::new("app.slow-trigger", delay));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, instance_id).await;
    join_instance(&mut b, instance_id).await;

    let entity_slow = orbisync_domain::EntityId::generate();
    let entity_fast = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity_slow, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn slow");
    expect_entity_applied(&mut a, entity_slow).await;
    b.send(Message::Binary(
        entity_command_bytes(entity_fast, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn fast");
    expect_entity_applied(&mut b, entity_fast).await;

    // A updates entity_slow with a hook call that sleeps for `delay`. Do not
    // wait for the response.
    a.send(Message::Binary(
        entity_command_bytes(
            entity_slow,
            1,
            "update",
            Some(update_args("app.slow-trigger", 1)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send slow update");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // B updates the unrelated entity_fast on the same instance. This must
    // complete well before `delay` elapses.
    let started = tokio::time::Instant::now();
    b.send(Message::Binary(
        entity_command_bytes(
            entity_fast,
            1,
            "update",
            Some(update_args("app.fast-key", 2)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send fast update");
    expect_entity_applied(&mut b, entity_fast).await;
    let elapsed = started.elapsed();
    assert!(
        elapsed < delay / 2,
        "an unrelated command on the same instance must not wait behind another \
         connection's pending hook call; took {elapsed:?}, slow hook delay is {delay:?}"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

/// A client resend of an already-admitted `command_id` must not call the
/// hook a second time — the existing `command_dedup` Leader/Duplicate
/// admission (`crates/orbisync-server/src/command_dedup.rs`, exercised
/// broadly by `command_dedup::tests`) is reused unchanged as the hook's
/// idempotency key (ADR-025 §2.4), rather than adding a second scheme.
#[tokio::test]
async fn duplicate_command_id_calls_the_hook_at_most_once() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let command_id = uuid::Uuid::now_v7().to_string();
    let bytes = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(pos)),
        command_id.clone(),
        3,
    );
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    // Resend the identical command_id (client retry after a lost ack, or a
    // network-level duplicate) in a new envelope with the next inbound
    // sequence number — the outer transport sequence always advances even
    // when the inner business-level command_id is retried.
    let resend = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(pos)),
        command_id.clone(),
        4,
    );
    a.send(Message::Binary(resend.into()))
        .await
        .expect("resend spawn");
    let replay = recv_envelope(&mut a).await.expect("replay response");
    assert!(
        matches!(replay.payload, Some(envelope::Payload::EntityCommand(_))),
        "the resend must replay the leader's result, got {:?}",
        replay.payload
    );
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        1,
        "a duplicate command_id must not call the hook a second time"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// A client can send any `expected_revision` it likes, including an
/// omitted/zero value that no longer matches the entity's real revision
/// (proto3 gives no wire-level distinction between "explicitly 0" and
/// "not set" for a plain `uint64` field). The hook receives that same
/// client-asserted value as `client_expected_revision` and may allow based on it,
/// but the final commit still goes through the actor's own
/// `ensure_matches(expected_revision)` (`crates/orbisync-world-runtime/src/actor.rs`)
/// against the *real* current revision — the same check that runs with no
/// hook configured at all. A hook Allow can never substitute for this
/// check for `update`/`delete`: it only widens the set of things Core
/// additionally consults, never narrows Core's own revision enforcement.
#[tokio::test]
async fn client_omitted_or_stale_revision_is_still_rejected_by_cores_own_check_despite_hook_allow()
{
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;
    // Spawn leaves the entity at revision 1. Sending `expected_revision: 0`
    // now is indistinguishable on the wire from a client that never set the
    // field, and it no longer matches reality.
    let calls_before = gate.calls.load(Ordering::SeqCst);

    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "update", Some(update_args("app.k", 1)), 4).into(),
    ))
    .await
    .expect("send update with stale/omitted revision");

    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "REVISION_MISMATCH",
        "Core's own optimistic-concurrency check must reject a stale/omitted \
         revision even though the hook allows every request"
    );
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        calls_before,
        "stale state is rejected before disclosure to the external hook"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// A hook Deny does not complete the command-dedup reservation (only an
/// applied/dispatched outcome does, at
/// `realtime_ws_connection_runtime.rs:1135` and after). Dropping the
/// reservation without completing it removes the pending entry
/// (`command_dedup.rs`'s `Drop for CommandDedupReservation` ->
/// `remove_pending`), so a resend of the same `command_id` is admitted as a
/// brand new Leader rather than replaying the old outcome or getting stuck
/// as permanently in-flight. This is the same release-on-early-exit
/// mechanism that would apply to any other early exit before dispatch
/// (including a future cancellation/shutdown path) — a Deny is simply the
/// one that is reachable and deterministic to test today. Contrast with
/// `duplicate_command_id_calls_the_hook_at_most_once`, which proves the
/// opposite for an *Allow* outcome (that one does complete, and a resend
/// replays without calling the hook again).
#[tokio::test]
async fn a_denied_commands_slot_is_released_so_a_resend_consults_the_hook_again() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Deny {
        reason: String::from("inside no-fly zone"),
    }));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let command_id = uuid::Uuid::now_v7().to_string();
    let first = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(pos)),
        command_id.clone(),
        3,
    );
    a.send(Message::Binary(first.into()))
        .await
        .expect("send spawn");
    let first_error = expect_error(&mut a).await;
    assert_eq!(first_error.code, "PRE_COMMIT_DENIED");
    assert_eq!(gate.calls.load(Ordering::SeqCst), 1);

    // Resend the identical command_id. If the dedup slot had leaked as
    // permanently "in flight" or cached the denial as a replayable
    // "Duplicate" outcome, this would either hang or short-circuit without
    // touching the hook. Neither happens: the entry was fully removed, so
    // this is admitted as a fresh Leader and the hook is consulted again.
    let resend = entity_command_bytes_with_id(
        entity,
        0,
        "spawn",
        Some(spawn_args_global(pos)),
        command_id,
        4,
    );
    a.send(Message::Binary(resend.into()))
        .await
        .expect("resend spawn");
    let second_error = expect_error(&mut a).await;
    assert_eq!(second_error.code, "PRE_COMMIT_DENIED");
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        2,
        "a resend of a denied command_id must not be treated as an already-admitted \
         Leader — the earlier reservation must have been released, not leaked"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

// --- Delete: basic allow/deny wiring, and the T6 delete-during-pending-hook race ---
//
// Independent verification (`docs/reviews/review-verification-2026-09-08-improvement-02-precommit-hook.md`)
// found that no test in this file ever dispatches a real `delete` operation
// over a real WebSocket connection: `PreCommitOperation::Delete` was only
// ever referenced in registration capability sets, never actually exercised
// end to end. The three tests below close that gap.

/// A real spawn followed by a real delete over a real WebSocket connection,
/// with the hook allowing: the delete is actually applied. State absence is
/// asserted, not just the broadcast — a follow-up update to the same entity
/// must fail with `ENTITY_NOT_FOUND`, proving the entity is really gone, not
/// merely that a `EntityCommand{operation: "delete"}` message was sent.
#[tokio::test]
async fn delete_allow_decision_actually_deletes_the_entity_end_to_end() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "delete", None, 4).into(),
    ))
    .await
    .expect("send delete");
    expect_entity_applied(&mut a, entity).await;
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        2,
        "the hook must be consulted for both the spawn and the delete"
    );

    // State-absence assertion: the entity must actually be gone, not just
    // broadcast as deleted. A follow-up update to the same entity_id must
    // fail with ENTITY_NOT_FOUND.
    a.send(Message::Binary(
        entity_command_bytes(entity, 2, "update", Some(update_args("app.k", 1)), 5).into(),
    ))
    .await
    .expect("send update after delete");
    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "ENTITY_NOT_FOUND",
        "the entity must actually be gone after an allowed delete, not merely broadcast as deleted"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// The same real spawn-then-delete path, but the hook denies the delete:
/// the command must be rejected before it reaches the actor, and the entity
/// must still exist afterward (state presence, not just the absence of a
/// broadcast). A follow-up update at the post-spawn revision must succeed.
#[tokio::test]
async fn delete_deny_decision_rejects_the_command_and_leaves_the_entity_intact() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(DenyOperationGate::new(
        PreCommitOperation::Delete,
        "deletion not allowed",
    ));
    let server = spawn_server(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "delete", None, 4).into(),
    ))
    .await
    .expect("send delete");
    let error = expect_error(&mut a).await;
    assert_eq!(error.code, "PRE_COMMIT_DENIED");
    assert_eq!(error.message, "deletion not allowed");

    // State-presence assertion: the entity must still exist. An update at
    // the post-spawn revision (1) must succeed, which is only possible if
    // the entity was never deleted.
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("app.k", 1)), 5).into(),
    ))
    .await
    .expect("send update after denied delete");
    expect_entity_applied(&mut a, entity).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// T6: while a slow hook call is pending for an update, a competing delete
/// from another connection completes first. When the delayed hook finally
/// allows the original update, the actor must reject it as
/// `ENTITY_NOT_FOUND` rather than applying an approval that assumed the
/// entity still existed — the same "stale approval must not be applied"
/// property `stale_revision_during_pending_hook_wait_is_not_applied` proves
/// for a revision race, here proven for a deletion race. `SelectiveDelayGate`
/// only delays requests whose `component_key` matches the marker; a
/// `delete` request has no `component_key` at all, so it is admitted
/// immediately while the update's hook call is still sleeping — the same
/// asymmetry a real network round trip would produce.
#[tokio::test]
async fn target_deleted_during_pending_hook_wait_is_not_applied() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let gate = Arc::new(SelectiveDelayGate::new(
        "app.slow-trigger",
        Duration::from_millis(400),
    ));
    let server = spawn_server_with_authorizer(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
        Arc::new(AllowAllWorldAuthorizer),
    )
    .await;

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
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;
    recv_envelope(&mut b).await.expect("b spawn echo");

    // A sends an update whose hook call will be held for 400ms. Do not wait
    // for A's response yet — it must stay in flight while B races ahead.
    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            1,
            "update",
            Some(update_args("app.slow-trigger", 1)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send slow update");
    tokio::time::sleep(Duration::from_millis(50)).await;

    // B deletes the same entity while A's hook call is still sleeping. The
    // delete's hook call has no component_key, so SelectiveDelayGate does
    // not delay it — it completes immediately.
    b.send(Message::Binary(
        entity_command_bytes(entity, 1, "delete", None, 3).into(),
    ))
    .await
    .expect("send delete");
    expect_entity_applied(&mut b, entity).await;

    // A's hook now returns Allow, but the entity no longer exists: the
    // stale approval must not be applied.
    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "ENTITY_NOT_FOUND",
        "a hook approval computed before a competing delete must not be applied \
         after the target no longer exists, got {}",
        error.code
    );
    assert_eq!(
        gate.calls.load(Ordering::SeqCst),
        3,
        "the hook must be reached by A's spawn, A's update, and B's delete"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
    #[allow(clippy::let_underscore_must_use)]
    let _ = b.close(None).await;
}

/// Independent verification (worker-brief §9, "instance退出" — reachable
/// case): this connection's own presence leaves the instance while its
/// command's hook call is still pending. Production has no live trigger
/// yet for removing a *connected* client's presence out from under it
/// (admin kick / forced disconnect is a separate, not-yet-implemented
/// capability — see the design memo), so this test drives the real
/// `InstanceCommand::Leave` path directly against the same presence this
/// connection joined with (captured from `JoinAccepted.presence_id`) to
/// model that trigger, then verifies the *detection and rejection*
/// mechanism itself: `RuntimeRegistry::is_present`, refreshed by the real
/// `Leave` command through the same cache `interest_views`/`member_count`
/// use (no actor round trip added to the check), and the WS layer's
/// pre/post-hook re-check wired in `realtime_ws_connection_runtime.rs`.
#[tokio::test]
async fn presence_lost_during_pending_hook_wait_is_not_applied() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();

    let dir_store = Arc::new(FakeWorldDirectoryStore::new());
    dir_store.insert_world(world);
    dir_store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(common::NoopCheckpointStore);
    let gate = Arc::new(SelectiveDelayGate::new(
        "app.slow-trigger",
        Duration::from_millis(300),
    ));
    let realtime_state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(dir_store as Arc<dyn WorldDirectoryStore>)
        .with_tickets(Arc::new(StubTicketVerifier::new()))
        .with_checkpoint_store(checkpoint_store)
        .with_world_authorizer(Arc::new(AllowAllWorldAuthorizer))
        .with_interest_grid(grid)
        .with_pre_commit_gate(Arc::clone(&gate) as Arc<dyn PreCommitGate>)
        .with_extension_registrations(
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>
        )
        .build(),
    );

    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(Arc::clone(&realtime_state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut a = ws_connect(addr).await;
    handshake(&mut a, TICKET).await;
    let join = join_instance_bytes(instance_id);
    a.send(Message::Binary(join.into()))
        .await
        .expect("send join");
    let first = recv_envelope(&mut a).await.expect("join response 1");
    let second = recv_envelope(&mut a).await.expect("join response 2");
    let presence_id = [&first, &second]
        .into_iter()
        .find_map(|env| match &env.payload {
            Some(envelope::Payload::JoinAccepted(accepted)) => Some(accepted.presence_id.clone()),
            _ => None,
        })
        .expect("JoinAccepted with a presence_id")
        .parse::<orbisync_domain::PresenceId>()
        .expect("valid presence id");

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            1,
            "update",
            Some(update_args("app.slow-trigger", 1)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send slow update");
    // Give the server time to admit the command and start the (delayed)
    // hook call before removing this connection's own presence.
    tokio::time::sleep(Duration::from_millis(50)).await;

    // Model an external presence-removal trigger (e.g. a future admin kick)
    // by submitting the real Leave command for this connection's own
    // presence while its update's hook call is still sleeping. The socket
    // itself stays open and keeps waiting for a response.
    registry
        .submit(instance_id, InstanceCommand::Leave { presence_id })
        .await
        .expect("leave submitted");

    let error = expect_error(&mut a).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;

    assert_eq!(
        error.code, "PRESENCE_LOST",
        "a presence that left the instance while its command's hook call was \
         pending must not have that command applied, got {}",
        error.code
    );
}

fn hmac_sha256_hex_digest(key: &[u8], message: &str) -> [u8; 32] {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(key).expect("hmac key");
    mac.update(message.as_bytes());
    mac.finalize().into_bytes().into()
}

/// Independent verification (worker-brief §9, "認証セッション失効"): drives
/// the real production ticket-verification path (`HmacRealtimeTicketVerifier`
/// + `RealtimeTicketStore`) and the real session lookup
/// (`IdentityRepository::find_session`), not a fake `RealtimeTicketVerifier`
/// that skips session tracking. Confirms both halves worker-brief §9 asked
/// for: a normal, still-active session's command commits, and the same
/// session revoked while a command's hook call is pending is caught before
/// commit — using `verify_with_session`'s new default-`None` compatibility
/// path only for verifiers that don't opt in (`StubTicketVerifier`,
/// `DenyAllTicketVerifier`, every other test in this file), which this test
/// does not rely on.
#[tokio::test]
async fn session_revoked_during_pending_hook_wait_is_not_applied_but_an_active_session_commits() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();

    let dir_store = Arc::new(FakeWorldDirectoryStore::new());
    dir_store.insert_world(world);
    dir_store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(common::NoopCheckpointStore);
    let gate = Arc::new(SelectiveDelayGate::new(
        "app.slow-trigger",
        Duration::from_millis(300),
    ));

    // Real production pieces: an HMAC-verified, single-use realtime ticket
    // bound to a real AuthSession, and a real (in-memory) IdentityRepository
    // for the session-status re-check to query.
    let hmac_key = b"precommit-hook-2-2-session-test-key".to_vec();
    let raw_ticket = "precommit-hook-2-2-session-test-ticket";
    let ticket_store = Arc::new(FakeRealtimeTicketStore::default());
    let identity_repository = Arc::new(FakeIdentityRepository::new());
    let user_id = orbisync_domain::UserId::generate();
    let session_id = orbisync_domain::AuthSessionId::generate();
    let issued_at = now_ts();
    let expires_at = Timestamp::from_unix_millis(1_700_000_000_000 + 3_600_000).expect("valid");
    let session = orbisync_domain::AuthSession::new(session_id, user_id, issued_at, expires_at)
        .expect("valid session");
    identity_repository
        .save_session(&session)
        .await
        .expect("save active session");
    ticket_store
        .create(CreateRealtimeTicketCommand {
            token_digest: hmac_sha256_hex_digest(&hmac_key, raw_ticket),
            session_id,
            user_id,
            issued_at,
            expires_at,
        })
        .await
        .expect("create ticket");
    let verifier = Arc::new(HmacRealtimeTicketVerifier::new(
        Arc::clone(&ticket_store) as Arc<dyn RealtimeTicketStore>,
        hmac_key,
        Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
    ));

    let realtime_state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(dir_store as Arc<dyn WorldDirectoryStore>)
        .with_tickets(verifier)
        .with_checkpoint_store(checkpoint_store)
        .with_world_authorizer(Arc::new(AllowAllWorldAuthorizer))
        .with_identity_repository(Arc::clone(&identity_repository) as Arc<dyn IdentityRepository>)
        .with_interest_grid(grid)
        .with_pre_commit_gate(Arc::clone(&gate) as Arc<dyn PreCommitGate>)
        .with_extension_registrations(
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>
        )
        .build(),
    );

    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(Arc::clone(&realtime_state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut a = ws_connect(addr).await;
    handshake(&mut a, raw_ticket).await;
    join_instance(&mut a, instance_id).await;

    // The session is still active: a normal spawn (whose hook call is not
    // delayed — a different component_key/operation than the marker below)
    // commits normally.
    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn with an active session");
    expect_entity_applied(&mut a, entity).await;

    // Now send an update whose hook call is held open, and revoke the
    // session while it is still sleeping.
    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            1,
            "update",
            Some(update_args("app.slow-trigger", 1)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send slow update");
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut revoked_session = session;
    revoked_session.revoke(now_ts());
    identity_repository
        .save_session(&revoked_session)
        .await
        .expect("save revoked session");

    let error = expect_error(&mut a).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;

    assert_eq!(
        error.code, "SESSION_REVOKED",
        "a session revoked while its command's hook call was pending must not \
         have that command applied, got {}",
        error.code
    );
}

/// Independent verification (worker-brief §8, priority item 1): a
/// permission revoked strictly *after* the hook call has already started
/// (confirmed via `SignalingDelayGate`), but *before* it resolves, must
/// still be caught — proving the re-check added after the hook's wait is
/// load-bearing, not merely a duplicate of a check already made before the
/// hook started. `authorizer.revoked` flips only once `started` is
/// observed true, so the pre-hook re-check (which necessarily runs before
/// `started` is set) cannot have been the one that caught this.
#[tokio::test]
async fn permission_revoked_strictly_after_hook_start_is_still_caught_before_commit() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let authorizer = Arc::new(RevocableAuthorizer::new());
    let started = Arc::new(AtomicBool::new(false));
    let gate = Arc::new(SignalingDelayGate::new(
        Arc::clone(&started),
        Duration::from_millis(200),
    ));
    let server = spawn_server_with_authorizer(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
        Arc::clone(&authorizer) as Arc<dyn orbisync_application::WorldAuthorizer>,
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("app.k", 42)), 4).into(),
    ))
    .await
    .expect("send update");

    // Wait until the hook call has provably started (its own pre-hook
    // permission re-check has already run and passed), then revoke.
    tokio::time::timeout(Duration::from_secs(2), async {
        while !started.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("hook call must start within 2s");
    authorizer.revoked.store(true, Ordering::SeqCst);

    // The hook still has ~200ms left to sleep before answering Allow. The
    // only re-check that can see the revocation is the one that runs after
    // the hook resolves.
    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "NOT_OWNER",
        "a permission revoked while the hook call was in flight must be caught \
         by the post-hook re-check before the command commits (the actor's \
         is_owner_allowed check reports this as NOT_OWNER once both \
         entity_update_own and entity_update_any are false), got {}",
        error.code
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// Independent verification (worker-brief §8, priority item 1), simpler
/// timing than `permission_revoked_strictly_after_hook_start_is_still_caught_before_commit`:
/// a permission revoked after the handshake but before the command carrying
/// a pending hook wait is even sent. This is the shape of the original
/// reproduction (`docs/reviews/repro-permission-revoked-during-pending-hook-wait.patch`)
/// converted into a regression test that now requires correct (blocking)
/// behavior instead of documenting the stale-grant bug.
#[tokio::test]
async fn permission_revoked_before_a_pending_hook_wait_is_not_applied_with_the_stale_grant() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let authorizer = Arc::new(RevocableAuthorizer::new());
    let gate = Arc::new(SelectiveDelayGate::new(
        "app.revoked-mid-wait",
        Duration::from_millis(200),
    ));
    let server = spawn_server_with_authorizer(
        world,
        instance,
        Some((
            Arc::clone(&gate) as Arc<dyn PreCommitGate>,
            Arc::new(SingleRegistrationStore::new()) as Arc<dyn ExtensionRegistrationStore>,
        )),
        Arc::clone(&authorizer) as Arc<dyn orbisync_application::WorldAuthorizer>,
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let calls_after_handshake = authorizer.calls.load(Ordering::SeqCst);
    assert!(
        calls_after_handshake > 0,
        "handshake must have resolved permissions via the authorizer at least once"
    );

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    // Simulate a live permission revocation happening after the grant Core
    // captured at handshake, before the next command (whose hook call will
    // then be held open) is even sent.
    authorizer.revoked.store(true, Ordering::SeqCst);

    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            1,
            "update",
            Some(update_args("app.revoked-mid-wait", 42)),
            4,
        )
        .into(),
    ))
    .await
    .expect("send update after revocation");
    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "NOT_OWNER",
        "a permission revoked before a pending hook wait must not let the stale \
         handshake-time grant commit the update, got {}",
        error.code
    );
    assert!(
        authorizer.calls.load(Ordering::SeqCst) > calls_after_handshake,
        "the authorizer must be re-consulted for this command, not just at handshake"
    );

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// Registration store whose reported registration can change after
/// construction — models a real registry where an operator activates a
/// spawn-hook Extension while the server is already running, as opposed to
/// `SingleRegistrationStore`'s fixed-at-construction registration. Drives
/// `RealtimeState.spawn_hook_active` and the live per-candidate check in
/// `realtime_ws_connection_runtime.rs` through the real registration path
/// (`find_active_registration_by_capability`), not by writing an
/// `AtomicBool` directly.
struct DynamicRegistrationStore {
    active: std::sync::Mutex<Option<ExtensionRegistration>>,
}

impl DynamicRegistrationStore {
    fn empty() -> Self {
        Self {
            active: std::sync::Mutex::new(None),
        }
    }

    fn activate_spawn_hook(&self) {
        *self.active.lock().expect("lock") = Some(ExtensionRegistration {
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("late-registered-zone-guard"),
            description: None,
            endpoint: String::from("https://zone-guard.example/precommit"),
            subscribed_events: BTreeSet::new(),
            capabilities: BTreeSet::from([String::from(PreCommitOperation::Spawn.capability())]),
            token_scopes: BTreeSet::new(),
            status: ExtensionStatus::Active,
            signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_ZONE_GUARD"),
        });
    }
}

#[async_trait::async_trait]
impl ExtensionRegistrationStore for DynamicRegistrationStore {
    async fn save_registration(
        &self,
        registration: ExtensionRegistration,
    ) -> Result<(), ApplicationError> {
        *self.active.lock().expect("lock") = Some(registration);
        Ok(())
    }

    async fn find_registration(
        &self,
        extension_id: uuid::Uuid,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        Ok(self
            .active
            .lock()
            .expect("lock")
            .clone()
            .filter(|r| r.extension_id == extension_id))
    }

    async fn find_active_registration_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        Ok(self
            .active
            .lock()
            .expect("lock")
            .clone()
            .filter(|r| r.capabilities.contains(capability)))
    }
}

/// Independent verification (worker-brief §9, gap A): a spawn hook
/// registered *after* the server has already booted (and before the next
/// periodic `spawn_hook_active` refresh, which in production runs every
/// 15s) must not leave a window where `UpdateTransform` auto-create still
/// bypasses it. Unlike the periodically-cached flag alone, the live
/// per-candidate lookup added to `realtime_ws_connection_runtime.rs`
/// checks the real registration store at the moment a genuinely new entity
/// is about to be auto-created, so this closes the window without ever
/// calling `refresh_spawn_hook_active_cache` between the registration and
/// the Transform.
///
/// Manual (not `spawn_server_with_authorizer`) setup: the test needs its
/// own handle to `RealtimeState` to demonstrate the periodic refresh is no
/// longer what makes this safe (it is never called between activation and
/// the Transform below).
#[tokio::test]
async fn spawn_hook_registered_after_boot_is_enforced_without_waiting_for_a_cache_refresh() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();

    let dir_store = Arc::new(FakeWorldDirectoryStore::new());
    dir_store.insert_world(world);
    dir_store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(common::NoopCheckpointStore);
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Deny {
        reason: String::from("would deny every explicit Spawn"),
    }));
    let registrations = Arc::new(DynamicRegistrationStore::empty());
    let realtime_state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(dir_store as Arc<dyn WorldDirectoryStore>)
        .with_tickets(Arc::new(StubTicketVerifier::new()))
        .with_checkpoint_store(checkpoint_store)
        .with_world_authorizer(Arc::new(AllowEntityOwnerAuthorizer))
        .with_interest_grid(grid)
        .with_pre_commit_gate(Arc::clone(&gate) as Arc<dyn PreCommitGate>)
        .with_extension_registrations(
            Arc::clone(&registrations) as Arc<dyn ExtensionRegistrationStore>
        )
        .build(),
    );
    // Mirrors main.rs: populate the cache once before serving connections.
    // The store has no registration yet, matching a server that boots
    // before any spawn hook is registered.
    realtime_state.refresh_spawn_hook_active_cache().await;

    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(Arc::clone(&realtime_state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    // An operator registers a spawn-hook Extension while the server is
    // already running, through the real write path.
    registrations.activate_spawn_hook();

    // Deliberately do NOT call refresh_spawn_hook_active_cache — this is
    // exactly the window between the registration landing in storage and
    // the next periodic tick (15s in production). The cached flag is still
    // stale (false) at this point.
    assert!(
        !realtime_state.spawn_hook_active.load(Ordering::Acquire),
        "the cached flag must still be stale immediately after activation, \
         which is the precondition this test's fix is meant to not depend on"
    );

    let mut a = ws_connect(addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let bytes = transform_input_bytes(entity, 3);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform during the stale-cache window");
    let error = expect_error(&mut a).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;

    assert_eq!(
        error.code, "EXPLICIT_SPAWN_REQUIRED",
        "a spawn hook registered after boot must be enforced against a new \
         auto-create candidate immediately, via the live per-candidate \
         lookup, without waiting for the next periodic cache refresh, got {}",
        error.code
    );
}

/// Registration store whose `find_active_registration_by_capability` fails
/// (e.g. the `Conflict` a duplicate active `hooks:entity:spawn`
/// registration produces, or a transient database error) until manually
/// switched to succeed.
struct FailingThenActiveRegistrationStore {
    fail: AtomicBool,
    registration: ExtensionRegistration,
}

impl FailingThenActiveRegistrationStore {
    fn new(registration: ExtensionRegistration) -> Self {
        Self {
            fail: AtomicBool::new(true),
            registration,
        }
    }

    fn stop_failing(&self) {
        self.fail.store(false, Ordering::SeqCst);
    }
}

#[async_trait::async_trait]
impl ExtensionRegistrationStore for FailingThenActiveRegistrationStore {
    async fn save_registration(
        &self,
        _registration: ExtensionRegistration,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }

    async fn find_registration(
        &self,
        _extension_id: uuid::Uuid,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        Ok(Some(self.registration.clone()))
    }

    async fn find_active_registration_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        if self.fail.load(Ordering::SeqCst) {
            return Err(ApplicationError::new(
                ApplicationErrorKind::Conflict,
                "simulated duplicate-registration or transient lookup failure",
            ));
        }
        Ok(self
            .registration
            .capabilities
            .contains(capability)
            .then(|| self.registration.clone()))
    }
}

/// Independent verification (worker-brief §9, gap B): if the periodic
/// `spawn_hook_active` refresh's underlying lookup fails (duplicate
/// registration `Conflict`, transient database error), auto-create must
/// not fail open just because the cached flag is stuck at its `false`
/// default. Both the cache's own error handling (`refresh_spawn_hook_active_cache`
/// now stores `true` on `Err`) and the live per-candidate lookup added to
/// `realtime_ws_connection_runtime.rs` (which independently fails closed
/// on the same `Err`) must reject the auto-create — this test drives the
/// real failing store through both paths rather than setting the
/// `AtomicBool` by hand.
#[tokio::test]
async fn spawn_hook_cache_refresh_failure_fails_closed_for_both_explicit_spawn_and_auto_create() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();

    let dir_store = Arc::new(FakeWorldDirectoryStore::new());
    dir_store.insert_world(world);
    dir_store.insert_instance(instance);
    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = UniformGrid::default();
    let clock = fixed_clock();
    let checkpoint_store: Arc<dyn CheckpointStore> = Arc::new(common::NoopCheckpointStore);
    let gate = Arc::new(ScriptedGate::new(PreCommitDecision::Allow));
    let registrations = Arc::new(FailingThenActiveRegistrationStore::new(
        ExtensionRegistration {
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("conflicted-zone-guard"),
            description: None,
            endpoint: String::from("https://zone-guard.example/precommit"),
            subscribed_events: BTreeSet::new(),
            capabilities: BTreeSet::from([String::from(PreCommitOperation::Spawn.capability())]),
            token_scopes: BTreeSet::new(),
            status: ExtensionStatus::Active,
            signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_ZONE_GUARD"),
        },
    ));
    let realtime_state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(dir_store as Arc<dyn WorldDirectoryStore>)
        .with_tickets(Arc::new(StubTicketVerifier::new()))
        .with_checkpoint_store(checkpoint_store)
        .with_world_authorizer(Arc::new(AllowEntityOwnerAuthorizer))
        .with_interest_grid(grid)
        .with_pre_commit_gate(Arc::clone(&gate) as Arc<dyn PreCommitGate>)
        .with_extension_registrations(
            Arc::clone(&registrations) as Arc<dyn ExtensionRegistrationStore>
        )
        .build(),
    );
    // Mirrors main.rs's boot-time refresh, but the store is primed to fail
    // this first lookup.
    realtime_state.refresh_spawn_hook_active_cache().await;
    assert!(
        realtime_state.spawn_hook_active.load(Ordering::Acquire),
        "a failed initial refresh must fail closed (true), not silently stay \
         at the false default"
    );

    let app = axum::Router::new()
        .route("/ws", axum::routing::get(realtime_ws_handler))
        .with_state(Arc::clone(&realtime_state));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        axum::serve(listener, app).await.expect("serve");
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut a = ws_connect(addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    // Explicit SpawnEntity fails closed via its own live (uncached) lookup.
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let explicit_spawn_entity = orbisync_domain::EntityId::generate();
    a.send(Message::Binary(
        entity_command_bytes(
            explicit_spawn_entity,
            0,
            "spawn",
            Some(spawn_args_global(pos)),
            3,
        )
        .into(),
    ))
    .await
    .expect("send explicit spawn");
    let error = expect_error(&mut a).await;
    assert_eq!(
        error.code, "PRE_COMMIT_UNAVAILABLE",
        "an explicit SpawnEntity must fail closed while the registration \
         lookup errors, got {}",
        error.code
    );

    // A bare UpdateTransform for an unknown entity must also fail closed —
    // both via the fail-closed cached flag and the live per-candidate
    // lookup added in this round.
    let auto_create_entity = orbisync_domain::EntityId::generate();
    let bytes = transform_input_bytes(auto_create_entity, 4);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform while the cache refresh keeps failing");
    let transform_error = expect_error(&mut a).await;
    assert!(
        transform_error.code == "EXPLICIT_SPAWN_REQUIRED"
            || transform_error.code == "PRE_COMMIT_UNAVAILABLE",
        "Transform auto-create must fail closed (not silently succeed) while \
         the registration lookup errors, got {}",
        transform_error.code
    );

    // Once the lookup recovers and a refresh runs, both paths keep working
    // correctly.
    registrations.stop_failing();
    realtime_state.refresh_spawn_hook_active_cache().await;

    let recovered_entity = orbisync_domain::EntityId::generate();
    let bytes = transform_input_bytes(recovered_entity, 5);
    a.send(Message::Binary(bytes.into()))
        .await
        .expect("send transform after the lookup recovers");
    let recovered_error = expect_error(&mut a).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;

    assert_eq!(
        recovered_error.code, "EXPLICIT_SPAWN_REQUIRED",
        "once the lookup recovers, auto-create must still be correctly \
         rejected (a real spawn hook is active), got {}",
        recovered_error.code
    );
}

// --- Real WS -> real HTTPS pre-commit gate -> real commit/deny (§7 point E) ---
//
// Every test above uses a fake `PreCommitGate` so the WS/dedup/actor wiring
// can be exercised deterministically. `crates/orbisync-extensions/src/precommit.rs`
// separately proves the real HTTP transport (TLS, SSRF egress policy, body
// parsing) against a real local HTTPS listener, but only at the gate's own
// unit-test layer, never through a real WebSocket connection. The tests
// below close that gap with one genuine, unmocked path: a real WS command
// triggers a real `ReqwestPreCommitClient` HTTP request (real TLS
// handshake, real HMAC-SHA-256 signature verified server-side, real JSON
// decision body) to a real local HTTPS listener, whose answer determines
// whether the command is actually committed.
//
// The listener trusts a certificate issued for this test run rather than
// disabling certificate verification (`ReqwestPreCommitClient::try_new_with_root_cert`),
// so this exercises the same certificate-validation code path production
// uses, not a weakened one.

const PRECOMMIT_HOOK_SECRET: &[u8] = b"integration-test-shared-secret";

fn hmac_hex(secret: &[u8], signing_input: &str) -> String {
    let mut mac = Hmac::<sha2::Sha256>::new_from_slice(secret).expect("hmac key");
    mac.update(signing_input.as_bytes());
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[derive(Clone, Copy)]
struct FixedSecret;

#[async_trait::async_trait]
impl SecretProvider for FixedSecret {
    async fn resolve(&self, _reference: &str) -> Result<Vec<u8>, DeliveryError> {
        Ok(PRECOMMIT_HOOK_SECRET.to_vec())
    }
}

struct LoopbackResolver {
    address: std::net::SocketAddr,
}

#[async_trait::async_trait]
impl DnsResolver for LoopbackResolver {
    async fn resolve(&self, _hostname: &str) -> Result<Vec<std::net::SocketAddr>, std::io::Error> {
        Ok(vec![self.address])
    }
}

/// A real local HTTPS listener speaking the exact pre-commit hook contract:
/// it verifies the HMAC-SHA-256 signature the same way
/// `examples/precommit-zone-guard/server.js` and `delivery::build_signed_webhook`
/// do, then answers `allow` or `deny` per `decision_body`. Requests with a
/// bad signature get a 401 with `{"decision":"deny"}` — a real, independent
/// verification path, not a stub that trusts every caller.
struct RealPreCommitServer {
    address: std::net::SocketAddr,
    root_cert_pem: Vec<u8>,
    task: tokio::task::JoinHandle<()>,
}

impl RealPreCommitServer {
    async fn start(decision_body: &'static str) -> Self {
        let _install = rustls::crypto::ring::default_provider().install_default();
        let cert = rcgen::generate_simple_self_signed(vec![String::from("precommit-e2e.test")])
            .expect("test certificate");
        let root_cert_der = cert.cert.der().to_vec();
        let root_cert_pem = cert.cert.pem().into_bytes();
        let certificate = rustls::pki_types::CertificateDer::from(root_cert_der.clone());
        let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
        );
        let config = rustls::ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(vec![certificate], key)
            .expect("test TLS configuration");
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("addr");
        let task = tokio::spawn(async move {
            loop {
                let Ok((socket, _peer)) = listener.accept().await else {
                    return;
                };
                let acceptor = acceptor.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let Ok(mut stream) = acceptor.accept(socket).await else {
                        return;
                    };
                    let mut buf = Vec::new();
                    let mut chunk = [0_u8; 4096];
                    let header_end = loop {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            break pos + 4;
                        }
                        if buf.len() > 64 * 1024 {
                            return;
                        }
                    };
                    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
                    let content_length: usize = header_text
                        .lines()
                        .find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.trim()
                                .eq_ignore_ascii_case("content-length")
                                .then(|| value.trim().parse().ok())
                                .flatten()
                        })
                        .unwrap_or(0);
                    while buf.len() < header_end + content_length {
                        let Ok(read) = stream.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        buf.extend_from_slice(&chunk[..read]);
                    }
                    let body = &buf[header_end..header_end + content_length];
                    let header_of = |name: &str| -> Option<String> {
                        header_text.lines().find_map(|line| {
                            let (key, value) = line.split_once(':')?;
                            key.trim()
                                .eq_ignore_ascii_case(name)
                                .then(|| value.trim().to_owned())
                        })
                    };
                    let event_id = header_of("X-OrbiSync-Event-Id").unwrap_or_default();
                    let timestamp = header_of("X-OrbiSync-Timestamp").unwrap_or_default();
                    let signature = header_of("X-OrbiSync-Signature").unwrap_or_default();
                    let signing_input =
                        format!("{timestamp}.{event_id}.{}", String::from_utf8_lossy(body));
                    let expected =
                        format!("sha256={}", hmac_hex(PRECOMMIT_HOOK_SECRET, &signing_input));
                    let (status_line, body_out) = if signature == expected {
                        ("200 OK", decision_body)
                    } else {
                        (
                            "401 Unauthorized",
                            r#"{"decision":"deny","reason":"bad signature"}"#,
                        )
                    };
                    let response = format!(
                        "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body_out}",
                        body_out.len()
                    );
                    let _ignore_write_error = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        Self {
            address,
            root_cert_pem,
            task,
        }
    }

    fn gate(&self, policy: PreCommitValidationPolicy) -> Arc<dyn PreCommitGate> {
        let resolver: Arc<dyn DnsResolver> = Arc::new(LoopbackResolver {
            address: self.address,
        });
        let client =
            ReqwestPreCommitClient::try_new_with_additional_ca(resolver, true, &self.root_cert_pem)
                .expect("real precommit client");
        Arc::new(PreCommitValidationGate::new(
            Arc::new(client),
            Arc::new(FixedSecret),
            policy,
        ))
    }
}

impl Drop for RealPreCommitServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn registration_pointing_at(endpoint: String) -> ExtensionRegistration {
    ExtensionRegistration {
        extension_id: uuid::Uuid::now_v7(),
        name: String::from("zone-guard-e2e"),
        description: None,
        endpoint,
        subscribed_events: BTreeSet::new(),
        capabilities: BTreeSet::from([
            String::from(PreCommitOperation::Spawn.capability()),
            String::from(PreCommitOperation::Update.capability()),
            String::from(PreCommitOperation::Delete.capability()),
        ]),
        token_scopes: BTreeSet::new(),
        status: ExtensionStatus::Active,
        signing_secret_ref: String::from("unused-by-FixedSecret"),
    }
}

struct FixedRegistrationStore(ExtensionRegistration);

#[async_trait::async_trait]
impl ExtensionRegistrationStore for FixedRegistrationStore {
    async fn save_registration(
        &self,
        _registration: ExtensionRegistration,
    ) -> Result<(), ApplicationError> {
        Ok(())
    }

    async fn find_registration(
        &self,
        _extension_id: uuid::Uuid,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        Ok(Some(self.0.clone()))
    }

    async fn find_active_registration_by_capability(
        &self,
        capability: &str,
    ) -> Result<Option<ExtensionRegistration>, ApplicationError> {
        if self.0.capabilities.contains(capability) {
            Ok(Some(self.0.clone()))
        } else {
            Ok(None)
        }
    }
}

/// A real spawn over a real WebSocket connection is validated by a real
/// HTTPS pre-commit extension (real TLS, real HMAC verification, real JSON
/// decision), and an `allow` answer results in the entity actually being
/// committed.
#[tokio::test]
async fn real_https_precommit_endpoint_allows_a_real_spawn_end_to_end() {
    let server_hook = RealPreCommitServer::start(r#"{"decision":"allow"}"#).await;
    let gate = server_hook.gate(PreCommitValidationPolicy::new(2_000, 4).unwrap());
    let registration =
        registration_pointing_at(String::from("https://precommit-e2e.test/precommit"));

    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(
        world,
        instance,
        Some((
            gate,
            Arc::new(FixedRegistrationStore(registration)) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");
    expect_entity_applied(&mut a, entity).await;

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// Same real HTTPS round trip, but the extension answers `deny` — the
/// command must be rejected before it ever reaches the actor, exactly as
/// the fake-gate `deny_decision_rejects_the_command_before_it_is_applied`
/// asserts, but this time with the real signing/TLS/HTTP path in between.
#[tokio::test]
async fn real_https_precommit_endpoint_denies_a_real_spawn_end_to_end() {
    let server_hook =
        RealPreCommitServer::start(r#"{"decision":"deny","reason":"inside no-fly zone"}"#).await;
    let gate = server_hook.gate(PreCommitValidationPolicy::new(2_000, 4).unwrap());
    let registration =
        registration_pointing_at(String::from("https://precommit-e2e.test/precommit"));

    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(
        world,
        instance,
        Some((
            gate,
            Arc::new(FixedRegistrationStore(registration)) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");

    let error = expect_error(&mut a).await;
    assert_eq!(error.code, "PRE_COMMIT_DENIED");
    assert_eq!(error.message, "inside no-fly zone");

    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// A real HTTPS pre-commit endpoint that never answers within the
/// configured policy timeout must fail closed, exactly like the timeout
/// path already proven with a fake gate at the transport-unit-test layer
/// (`precommit::tests::timeout_is_denied_and_does_not_wait_past_the_policy_bound`),
/// but here verified through the real server-wired command path: the
/// client gets a controlled rejection and no state change, not a hang.
#[tokio::test]
async fn real_https_precommit_endpoint_timeout_denies_and_leaves_state_unchanged() {
    // A listener that accepts the TCP/TLS connection but never writes a
    // response models a hung extension process, as distinct from a
    // connection refused (which the transport layer already treats as an
    // error, not a timeout).
    let _install = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec![String::from("precommit-e2e-hang.test")])
        .expect("test certificate");
    let root_cert_der = cert.cert.der().to_vec();
    let certificate = rustls::pki_types::CertificateDer::from(root_cert_der.clone());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        cert.key_pair.serialize_der(),
    ));
    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate], key)
        .expect("test TLS configuration");
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let address = listener.local_addr().expect("addr");
    let hang_task = tokio::spawn(async move {
        loop {
            let Ok((socket, _peer)) = listener.accept().await else {
                return;
            };
            let acceptor = acceptor.clone();
            // Accept the TLS handshake, then read and never respond.
            tokio::spawn(async move {
                let Ok(mut stream) = acceptor.accept(socket).await else {
                    return;
                };
                use tokio::io::AsyncReadExt;
                let mut sink = [0_u8; 4096];
                loop {
                    match stream.read(&mut sink).await {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                }
            });
        }
    });

    let resolver: Arc<dyn DnsResolver> = Arc::new(LoopbackResolver { address });
    let client = ReqwestPreCommitClient::try_new_with_root_cert(resolver, true, &root_cert_der)
        .expect("real precommit client");
    let gate: Arc<dyn PreCommitGate> = Arc::new(PreCommitValidationGate::new(
        Arc::new(client),
        Arc::new(FixedSecret),
        PreCommitValidationPolicy::new(300, 4).unwrap(),
    ));
    let registration =
        registration_pointing_at(String::from("https://precommit-e2e-hang.test/precommit"));

    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let instance_id = instance.id();
    let server = spawn_server(
        world,
        instance,
        Some((
            gate,
            Arc::new(FixedRegistrationStore(registration)) as Arc<dyn ExtensionRegistrationStore>,
        )),
    )
    .await;

    let mut a = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    join_instance(&mut a, instance_id).await;

    let entity = orbisync_domain::EntityId::generate();
    let pos = orbisync_domain::Vec3::new(1.0, 0.0, 0.0).expect("vec");
    let started = std::time::Instant::now();
    a.send(Message::Binary(
        entity_command_bytes(entity, 0, "spawn", Some(spawn_args_global(pos)), 3).into(),
    ))
    .await
    .expect("send spawn");

    let error = expect_error(&mut a).await;
    assert_eq!(error.code, "PRE_COMMIT_DENIED");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "a hung extension must be bounded by the policy timeout (300ms), not left to hang"
    );

    hang_task.abort();
    #[allow(clippy::let_underscore_must_use)]
    let _ = a.close(None).await;
}

/// Rule state is read from Core, not from client claims or an external store.
#[derive(Default)]
struct CoreStateRule {
    requests: std::sync::Mutex<Vec<PreCommitValidationRequest>>,
}

#[async_trait::async_trait]
impl PreCommitGate for CoreStateRule {
    async fn validate(
        &self,
        _: &ExtensionRegistration,
        request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        self.requests.lock().unwrap().push(request.clone());
        let current = request.current_entity.as_ref();
        let locked = current
            .and_then(|v| v.pointer("/components/app.rule/value/value"))
            .and_then(|v| v.as_f64())
            == Some(1.0);
        if locked {
            let owner = current
                .and_then(|v| v.get("owner_id"))
                .and_then(|v| v.as_str());
            if request.operation != PreCommitOperation::Update
                || request.component_key.as_deref() != Some("app.rule")
                || request.payload.get("value").and_then(|v| v.as_f64()) != Some(0.0)
                || owner != Some(request.requester.to_string().as_str())
            {
                return PreCommitDecision::Deny {
                    reason: "locked in Core".into(),
                };
            }
        }
        PreCommitDecision::Allow
    }
}

#[tokio::test]
async fn authoritative_core_state_enforces_lock_and_owner_unlock_without_external_store() {
    let world = make_world();
    let instance = make_instance(world.id(), 100);
    let id = instance.id();
    let gate = Arc::new(CoreStateRule::default());
    let server = spawn_server_with_authorizer(
        world,
        instance,
        Some((gate.clone(), Arc::new(SingleRegistrationStore::new()))),
        Arc::new(AllowAllWorldAuthorizer),
    )
    .await;
    let mut a = ws_connect(server.addr).await;
    let mut b = ws_connect(server.addr).await;
    handshake(&mut a, TICKET).await;
    handshake(&mut b, TICKET).await;
    join_instance(&mut a, id).await;
    join_instance(&mut b, id).await;
    let entity = orbisync_domain::EntityId::generate();
    a.send(Message::Binary(
        entity_command_bytes(
            entity,
            0,
            "spawn",
            Some(spawn_args_global(
                orbisync_domain::Vec3::new(1.0, 0.0, 0.0).unwrap(),
            )),
            3,
        )
        .into(),
    ))
    .await
    .unwrap();
    expect_entity_applied(&mut a, entity).await;
    expect_entity_applied(&mut b, entity).await;
    assert!(gate.requests.lock().unwrap()[0].current_entity.is_none());
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("app.rule", 1)), 4).into(),
    ))
    .await
    .unwrap();
    expect_entity_applied(&mut a, entity).await;
    expect_entity_applied(&mut b, entity).await;
    b.send(Message::Binary(
        entity_command_bytes(entity, 2, "update", Some(update_args("app.rule", 0)), 3).into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut b).await.code, "PRE_COMMIT_DENIED");
    a.send(Message::Binary(
        entity_command_bytes(entity, 2, "delete", None, 5).into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut a).await.code, "PRE_COMMIT_DENIED");
    a.send(Message::Binary(
        entity_command_bytes(entity, 1, "update", Some(update_args("app.rule", 0)), 6).into(),
    ))
    .await
    .unwrap();
    assert_eq!(expect_error(&mut a).await.code, "REVISION_MISMATCH");
    a.send(Message::Binary(
        entity_command_bytes(entity, 2, "update", Some(update_args("app.rule", 0)), 7).into(),
    ))
    .await
    .unwrap();
    expect_entity_applied(&mut a, entity).await;
    expect_entity_applied(&mut b, entity).await;
    a.send(Message::Binary(
        entity_command_bytes(entity, 3, "delete", None, 8).into(),
    ))
    .await
    .unwrap();
    expect_entity_applied(&mut a, entity).await;
    let requests = gate.requests.lock().unwrap();
    assert_eq!(requests.len(), 6, "stale command must not call the rule");
    assert_eq!(requests[2].current_entity.as_ref().unwrap()["revision"], 2);
    assert_eq!(
        requests[2].current_entity.as_ref().unwrap()["components"]["app.rule"]["value"]["value"]
            .as_f64(),
        Some(1.0)
    );
    assert_eq!(
        requests[5].current_entity.as_ref().unwrap()["components"]["app.rule"]["value"]["value"]
            .as_f64(),
        Some(0.0)
    );
}
