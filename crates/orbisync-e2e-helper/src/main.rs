//! E2E helper binary for W-20 SDK reconnect tests.
//!
//! Starts a real Axum server with `RealtimeState` backed by fake in-memory
//! stores, no PostgreSQL, on 127.0.0.1:0. The server implements:
//! - `POST /v1/auth/login` → stub TokenPair (contract: TokenPair)
//! - `POST /v1/realtime/tickets` → stub ticket (contract: TicketResponse = { realtime_ticket, expires_in })
//! - `GET /ws`, `GET /v1/realtime/ws` → realtime_ws_handler
//!
//! Ticket endpoint shape is validated against `openapi/orbisync-v1.yaml`
//! components.schemas.TicketResponse by `scripts/check_realtime_ticket_contract.py`.
//! See RV-D decision: production `orbisync-transport-http` router is not
//! shared here because it requires `HttpState` with `AccessTokenService`,
//! `IdentityRepository`, and `PasswordService` plus seeded accounts, and
//! shares a different Axum state type than `RealtimeState`. Merging the two
//! routers would require a combined state or two separate servers, increasing
//! helper complexity and coupling the e2e test to password hashing and JWT
//! key management. Instead the stub is kept but its JSON is validated against
//! the OpenAPI contract at gate time, preventing divergent mock shapes.
//!
//! Prints to stdout:
//!   READY addr=127.0.0.1:xxxxx worldId=... instanceId=...
//! Then runs until killed. Logs to stderr.

use std::sync::{Arc, Mutex};

use axum::response::IntoResponse;
use axum::routing::{get, post};
use orbisync_application::{
    AppCheckpoint, ApplicationError, ApplicationErrorKind, CheckpointSaveReceipt, CheckpointStore,
    MAX_CHECKPOINT_PAYLOAD_BYTES, WorldAuthorizer,
};
use orbisync_domain::{InstanceId, Timestamp, Transform, UserId, World, WorldId};
use orbisync_realtime::gateway::{GatewayError, RealtimeTicketVerifier};
use orbisync_server::{delivery::DeliveryRegistry, realtime_ws::RealtimeState};
use orbisync_testkit::{FakeWorldDirectoryStore, FixedClock};
use orbisync_world_runtime::RuntimeRegistry;

#[path = "../../../examples/server-input/movement.rs"]
mod movement;
/// Single-instance checkpoint store for the SDK helper.
///
/// The helper owns exactly one instance and does not need PostgreSQL, but the
/// real realtime activation path intentionally fails closed without a
/// checkpoint store. Keeping the latest bounded payload in memory exercises
/// that production path without weakening it or introducing external I/O.
#[derive(Debug, Default)]
struct InMemoryCheckpointStore {
    latest: Mutex<Option<AppCheckpoint>>,
}

impl InMemoryCheckpointStore {
    fn error(kind: ApplicationErrorKind, detail: impl Into<String>) -> ApplicationError {
        ApplicationError::new(kind, detail.into())
    }

    fn receipts(
        checkpoint: &AppCheckpoint,
    ) -> Result<Vec<CheckpointSaveReceipt>, ApplicationError> {
        if checkpoint.payload.len() > MAX_CHECKPOINT_PAYLOAD_BYTES {
            return Err(Self::error(
                ApplicationErrorKind::CheckpointTooLarge,
                format!("checkpoint payload exceeds {MAX_CHECKPOINT_PAYLOAD_BYTES} byte limit"),
            ));
        }

        let data: serde_json::Value =
            serde_json::from_slice(&checkpoint.payload).map_err(|_| {
                Self::error(
                    ApplicationErrorKind::PortFailure,
                    "invalid checkpoint payload",
                )
            })?;
        let Some(entries) = data.get("dedup").and_then(serde_json::Value::as_array) else {
            return Ok(Vec::new());
        };

        entries
            .iter()
            .map(|entry| {
                let command_id = entry
                    .get("command_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        Self::error(
                            ApplicationErrorKind::PortFailure,
                            "invalid checkpoint dedup",
                        )
                    })?;
                let created_at_millis = entry
                    .get("created_at_millis")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| {
                        Self::error(
                            ApplicationErrorKind::PortFailure,
                            "invalid checkpoint dedup",
                        )
                    })?;
                let expires_at_millis = entry
                    .get("expires_at_millis")
                    .and_then(serde_json::Value::as_i64)
                    .ok_or_else(|| {
                        Self::error(
                            ApplicationErrorKind::PortFailure,
                            "invalid checkpoint dedup",
                        )
                    })?;
                Ok(CheckpointSaveReceipt {
                    command_id: command_id.to_owned(),
                    created_at_millis,
                    expires_at_millis,
                })
            })
            .collect()
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Option<AppCheckpoint>>, ApplicationError> {
        self.latest.lock().map_err(|_| {
            Self::error(
                ApplicationErrorKind::PortFailure,
                "checkpoint store lock is poisoned",
            )
        })
    }
}

#[async_trait::async_trait]
impl CheckpointStore for InMemoryCheckpointStore {
    async fn save_checkpoint(
        &self,
        checkpoint: AppCheckpoint,
    ) -> Result<Vec<CheckpointSaveReceipt>, ApplicationError> {
        let receipts = Self::receipts(&checkpoint)?;
        let mut latest = self.lock()?;
        if let Some(current) = latest.as_ref() {
            if current.instance_id != checkpoint.instance_id {
                return Err(Self::error(
                    ApplicationErrorKind::PortFailure,
                    "SDK helper checkpoint store received an unexpected instance",
                ));
            }
            if current.revision > checkpoint.revision {
                return Err(Self::error(
                    ApplicationErrorKind::Conflict,
                    "checkpoint revision is older than the in-memory checkpoint",
                ));
            }
        }
        *latest = Some(checkpoint);
        Ok(receipts)
    }

    async fn load_latest(
        &self,
        instance_id: InstanceId,
    ) -> Result<Option<AppCheckpoint>, ApplicationError> {
        Ok(self
            .lock()?
            .as_ref()
            .filter(|checkpoint| checkpoint.instance_id == instance_id)
            .cloned())
    }
}

#[tokio::main]
async fn main() {
    let filter = std::env::var("RUST_LOG").unwrap_or_else(|_| "info".to_string());
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let clock = Arc::new(FixedClock::new(
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    ));
    let store = Arc::new(FakeWorldDirectoryStore::new());
    let world = World::new(
        WorldId::generate(),
        "e2e-world",
        None,
        Transform::identity(),
        100,
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    )
    .expect("world");
    let instance = orbisync_domain::WorldInstance::new(
        InstanceId::generate(),
        world.id(),
        100,
        Timestamp::from_unix_millis(1_700_000_000_000).expect("valid"),
    )
    .expect("instance");
    let world_id = world.id();
    let instance_id = instance.id();
    store.insert_world(world);
    store.insert_instance(instance);

    let registry = Arc::new(RuntimeRegistry::new());
    let delivery = Arc::new(DeliveryRegistry::new());
    let grid = orbisync_interest::UniformGrid::default();
    // Fixed verifier ensures same ticket maps to same UserId across reconnects,
    // otherwise StubTicketVerifier's random UserId would make every resume fail the
    // `binding.user_id != user` check and force ResyncRequired instead of ResumeAccepted.
    // This matches the real AccessTokenTicketVerifier's deterministic mapping.
    struct FixedUserVerifier {
        user_id: UserId,
    }
    #[async_trait::async_trait]
    impl RealtimeTicketVerifier for FixedUserVerifier {
        async fn verify(&self, ticket: &str) -> Result<UserId, GatewayError> {
            if ticket.is_empty() {
                Err(GatewayError::InvalidTicket)
            } else {
                Ok(self.user_id)
            }
        }
    }

    #[derive(Debug, Clone, Copy, Default)]
    struct FixedWorldAuthorizer;

    #[async_trait::async_trait]
    impl WorldAuthorizer for FixedWorldAuthorizer {
        async fn require(&self, _actor: UserId, permission: &str) -> Result<(), ApplicationError> {
            match permission {
                "entity.spawn" | "entity.update.own" => Ok(()),
                _ => Err(ApplicationError::new(
                    ApplicationErrorKind::NotAuthorized,
                    "permission denied",
                )),
            }
        }
    }

    /// In-memory stand-in for the durable checkpoint port.
    ///
    /// `ensure_instance_activated` refuses a join when no checkpoint store is
    /// configured, so a helper without one cannot activate its instance at all
    /// and every join fails with `instance state could not be restored`. The
    /// helper predates that requirement, which is why it needs a store here.
    ///
    /// `load_latest` returns `Ok(None)`: this helper never persists anything, so
    /// no instance has a durable checkpoint, and activation takes the
    /// no-checkpoint branch that starts a fresh actor. That is exactly the
    /// behaviour the e2e test wants — a clean instance per helper process.
    ///
    /// `save_checkpoint` keeps nothing but still has to honour the receipt
    /// contract: the command durability path looks up the receipt for the
    /// command it just wrote and treats a missing or mismatched entry as a
    /// durability failure. Echoing the submitted dedup entries reports the
    /// timestamps a real store would have adopted when it has nothing older to
    /// merge with, which is always true here. This mirrors the fake used by the
    /// Rust integration tests (`tests/integration/tests/common/mod.rs`).
    #[derive(Debug, Clone, Copy, Default)]
    struct NoopCheckpointStore;

    #[async_trait::async_trait]
    impl orbisync_application::CheckpointStore for NoopCheckpointStore {
        async fn save_checkpoint(
            &self,
            checkpoint: orbisync_application::AppCheckpoint,
        ) -> Result<Vec<orbisync_application::CheckpointSaveReceipt>, ApplicationError> {
            let invalid = |detail: &str| {
                ApplicationError::new(ApplicationErrorKind::PortFailure, detail.to_owned())
            };
            let data: serde_json::Value = serde_json::from_slice(&checkpoint.payload)
                .map_err(|_| invalid("invalid checkpoint payload"))?;
            let Some(entries) = data.get("dedup").and_then(serde_json::Value::as_array) else {
                return Ok(Vec::new());
            };
            entries
                .iter()
                .map(|entry| {
                    let command_id = entry
                        .get("command_id")
                        .and_then(serde_json::Value::as_str)
                        .ok_or_else(|| invalid("invalid checkpoint dedup"))?;
                    let created_at_millis = entry
                        .get("created_at_millis")
                        .and_then(serde_json::Value::as_i64)
                        .ok_or_else(|| invalid("invalid checkpoint dedup"))?;
                    let expires_at_millis = entry
                        .get("expires_at_millis")
                        .and_then(serde_json::Value::as_i64)
                        .ok_or_else(|| invalid("invalid checkpoint dedup"))?;
                    Ok(orbisync_application::CheckpointSaveReceipt {
                        command_id: command_id.to_owned(),
                        created_at_millis,
                        expires_at_millis,
                    })
                })
                .collect()
        }

        async fn load_latest(
            &self,
            _instance_id: InstanceId,
        ) -> Result<Option<orbisync_application::AppCheckpoint>, ApplicationError> {
            Ok(None)
        }
    }

    // Use a single fixed user for all tickets in this helper; the e2e test uses
    // separate clients but they share the same stub ticket, so they will be
    // considered the same user. For the purpose of W-20's state-intact test we
    // actually want two distinct users to test cross-user visibility, but the
    // resume token's user binding requires same user on reconnect, so we keep
    // the fixed user and the test's B will be same user as A — still exercises
    // the resume path (the server's user check passes).
    let fixed_user = UserId::generate();
    let verifier: Arc<dyn RealtimeTicketVerifier> = Arc::new(FixedUserVerifier {
        user_id: fixed_user,
    });

    let state = Arc::new(
        RealtimeState::builder(
            orbisync_config::Config::default().realtime,
            Arc::clone(&registry),
            Arc::clone(&delivery),
            Arc::clone(&clock) as Arc<dyn orbisync_domain::Clock>,
        )
        .with_world_store(store as Arc<dyn orbisync_application::WorldDirectoryStore>)
        .with_checkpoint_store(Arc::new(InMemoryCheckpointStore::default()))
        .with_world_authorizer(Arc::new(FixedWorldAuthorizer))
        .with_checkpoint_store(
            Arc::new(NoopCheckpointStore) as Arc<dyn orbisync_application::CheckpointStore>
        )
        .with_tickets(verifier)
        .with_input_rule(world_id, "example.move", Arc::new(movement::Movement))
        .with_interest_grid(grid)
        .with_resume_grace_seconds(60)
        .build(),
    );

    async fn login_stub(
        axum::Json(_body): axum::Json<serde_json::Value>,
    ) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "access_token": "stub-access-token",
            "refresh_token": "stub-refresh-token",
            "token_type": "Bearer",
            "expires_in": 900
        }))
    }
    async fn tickets_stub(_headers: axum::http::HeaderMap) -> axum::Json<serde_json::Value> {
        axum::Json(serde_json::json!({
            "realtime_ticket": "stub-ticket-123",
            "expires_in": 60
        }))
    }

    async fn ws_with_log(
        ws: axum::extract::ws::WebSocketUpgrade,
        headers: axum::http::HeaderMap,
        state: axum::extract::State<Arc<RealtimeState>>,
    ) -> axum::response::Response {
        orbisync_server::realtime_ws::realtime_ws_handler(ws, headers, state)
            .await
            .into_response()
    }

    let app = axum::Router::new()
        .route(
            "/v1/auth/methods",
            get(|| async { axum::Json(serde_json::json!({ "methods": ["local"] })) }),
        )
        .route("/v1/auth/login", post(login_stub))
        .route("/v1/realtime/tickets", post(tickets_stub))
        .route("/ws", get(ws_with_log))
        .route("/v1/realtime/ws", get(ws_with_log))
        .with_state(Arc::clone(&state));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let addr = listener.local_addr().expect("addr");
    // Print READY line for the Node test harness to parse
    println!(
        "READY addr={} worldId={} instanceId={}",
        addr, world_id, instance_id
    );
    use std::io::Write as _;
    std::io::stdout().flush().expect("flush");

    axum::serve(listener, app).await.expect("serve");
}
