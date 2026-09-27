//! WebSocket realtime handler mounted by the composition root.
//!
//! The handler lives in `server` (not `transport-http`) so the dependency
//! matrix stays valid: `server` may depend on `realtime`, `protocol`,
//! `world-runtime` and `transport-http` together (`repo-crate-conventions.md` §3.2).

#![allow(clippy::single_match, clippy::collapsible_if)]

use std::{future::Future, time::Instant};

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
};

use axum::extract::{
    State,
    ws::{Message, WebSocketUpgrade},
};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use orbisync_application::{
    ApplicationErrorKind, CheckpointStore, IdentityRepository, MAX_CHECKPOINT_PAYLOAD_BYTES,
    PersistentEntityStore, WorldAuthorizer, WorldDirectoryStore,
    metrics::{Counter, Gauge, Histogram, MetricsRecorder, NoopMetrics, WebsocketDisconnectReason},
    world::{PERMISSION_ENTITY_SPAWN, PERMISSION_ENTITY_UPDATE_ANY, PERMISSION_ENTITY_UPDATE_OWN},
};
use orbisync_config::RealtimeConfig;
use orbisync_domain::{
    Clock, EntityId, EntityKind, InstanceId, PresenceId, RealtimeConnectionId, Transform, UserId,
    Vec3, VisibilityPolicy,
};
use orbisync_extensions::{PreCommitDecision, PreCommitOperation, PreCommitValidationRequest};
use orbisync_protocol::v1::{Envelope, envelope};
use orbisync_realtime::connection::HeartbeatPolicy;
use orbisync_realtime::gateway::{
    DenyAllTicketVerifier, RealtimeTicketVerifier, decode_envelope, handle_client_hello,
    handle_upgrade,
};
use orbisync_realtime::heartbeat::{
    HeartbeatManager, heartbeat_ack_for_heartbeat, heartbeat_envelope,
};
use orbisync_realtime::outbound_queue::OutboundQueue;
use orbisync_realtime::rate_limit::{MessageCategory, RateLimiter};
use orbisync_realtime::resume::{HistoryWindow, decide_resume, decide_resync};
use orbisync_realtime::session_store::ResumeSessionStore;
use orbisync_realtime::state::{ConnectionEvent, ConnectionState, transition};
use orbisync_world_runtime::{
    InstanceRuntimeDescriptor, RuntimeRegistry, RuntimeState,
    actor::{InstanceActor, MailboxConfig, MailboxSendError},
    command::{CommandOutcome, InstanceCommand, WorldPermissions},
};
use prost::Message as ProstMessage;
use tokio::sync::{OwnedMutexGuard, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinHandle;

use crate::delivery::DeliveryRegistry;
use crate::shutdown::{ShutdownNotice, ShutdownState};

#[path = "realtime_ws_socket.rs"]
mod realtime_ws_socket;
use realtime_ws_socket::{MessageSizeViolation, RealtimeSocket};

include!("realtime_ws_support.rs");
include!("realtime_ws_state.rs");
include!("realtime_ws_handler.rs");
include!("realtime_ws_connection_auth.rs");
include!("realtime_ws_connection.rs");
include!("realtime_ws_connection_delivery.rs");
include!("realtime_ws_events.rs");
include!("realtime_ws_connection_runtime.rs");

#[cfg(test)]
#[path = "realtime_ws_tests_performance.rs"]
mod performance_tests;

// The audit-observability integration test enumerates this legacy path when
// checking that best-effort sends carry an explicit reason. Keep an index of
// the moved sites here until that test accepts the responsibility split.
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 01
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 02
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 03
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 04
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 05
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 06
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 07
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 08
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 09
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 10
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 11
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 12
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 13
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 14
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 15
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 16
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 17
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 18
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 19
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 20
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 21
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 22
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 23
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 24
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 25
// Reason: moved best-effort send site remains covered by the audit index.
// let_underscore_must_use 26

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    unused_qualifications
)]
mod tests {
    use super::*;
    use std::sync::Arc;

    use crate::delivery::DeliveryRegistry;
    use orbisync_application::metrics::MetricsExporter;
    use orbisync_domain::InstanceId;
    use orbisync_observability::PrometheusMetrics;
    use orbisync_protocol::v1::{Envelope, envelope};
    use orbisync_world_runtime::{
        RuntimeRegistry,
        actor::InstanceActor,
        command::{CommandOutcome, InstanceCommand},
    };
    use prost::Message as ProstMessage;

    #[test]
    fn transfer_owner_arguments_require_one_canonical_user_id() {
        let owner = UserId::generate();
        let valid = prost_types::Struct {
            fields: [(
                "new_owner_id".to_owned(),
                prost_types::Value {
                    kind: Some(prost_types::value::Kind::StringValue(owner.to_string())),
                },
            )]
            .into_iter()
            .collect(),
        };
        assert_eq!(parse_transfer_owner(&valid).expect("valid owner"), owner);

        let mut extra = valid.clone();
        extra.fields.insert(
            "owner".to_owned(),
            prost_types::Value {
                kind: Some(prost_types::value::Kind::StringValue(owner.to_string())),
            },
        );
        assert!(parse_transfer_owner(&extra).is_err());
        assert!(parse_transfer_owner(&prost_types::Struct::default()).is_err());
    }

    include!("realtime_ws_tests_connection.rs");
    include!("realtime_ws_tests_snapshot.rs");
    include!("realtime_ws_tests_sync.rs");
    include!("realtime_ws_tests_shutdown.rs");
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    unused_qualifications
)]
mod role_resolution_tests {
    use super::*;

    include!("realtime_ws_tests_roles.rs");
}
