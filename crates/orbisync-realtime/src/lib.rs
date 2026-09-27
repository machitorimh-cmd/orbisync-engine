//! Realtime gateway, delivery and presence.
//!
//! Owns WebSocket connections, the connection registry, heartbeat, resume, the
//! bounded outbound queue and protocol frame handling (specification §30.4,
//! `architecture.md` §3). It never owns world domain state.
//!
//! # Dependency rule
//!
//! `realtime` depends on `domain`, `application`, `protocol` and `config`
//! (`repo-crate-conventions.md` §3.2). `world-runtime` and `interest` must not
//! depend on it (§8 acceptance conditions 4 and 5).
//!
//! Milestones 2 and 3 implement the socket handling. Milestone 0 encodes the
//! connection state machine, whose source of truth is
//! `contracts/realtime-connection-state-machine.json`; the contract test in
//! `tests/state_machine_contract.rs` fails if code and contract drift apart.

pub mod connection;
pub mod gateway;
pub mod heartbeat;
pub mod outbound_queue;
pub mod rate_limit;
pub mod resume;
pub mod sequence;
pub mod session_store;
pub mod state;

pub use connection::{ConnectionDescriptor, HeartbeatPolicy, binary_subprotocol};
pub use gateway::{
    DenyAllTicketVerifier, GatewayError, HandshakeSuccess, RealtimeTicketVerifier,
    StubTicketVerifier, build_server_hello_envelope, check_message_size,
    check_message_size_for_envelope, decode_envelope, encode_envelope, gateway_error_to_event,
    handle_client_hello, handle_client_hello_with_config, handle_upgrade, message_size_limit,
    negotiate_minor_for_hello, next_state_for_error, parse_client_hello, validate_major,
    validate_subprotocol, validate_ticket,
};
pub use heartbeat::{
    HeartbeatManager, heartbeat_ack_envelope, heartbeat_ack_for_heartbeat, heartbeat_envelope,
};
pub use outbound_queue::{
    DEFAULT_CAPACITY, LatestEnqueueResult, OutboundQueue, QueueDepth, QueueError,
};
pub use rate_limit::{MessageCategory, RateLimitError, RateLimiter, TokenBucket};
pub use resume::{
    HistoryWindow, RESUME_TOKEN_MISMATCH, ResumeCase, ResumeDecision, ResumeOutcome, ResyncOutcome,
    ResyncReason, constant_time_eq, constant_time_eq_bytes, decide_resume,
    decide_resume_from_strings, decide_resync, decide_resync_u64, is_token_present,
};
pub use sequence::{InboundSequence, InboundSequenceTracker};
pub use session_store::{ResumeBinding, ResumeSessionStore};
pub use state::{ConnectionEvent, ConnectionState, TRANSITIONS, transition};
