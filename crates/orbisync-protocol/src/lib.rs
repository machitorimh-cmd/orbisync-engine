//! Realtime wire contract.
//!
//! `proto/orbisync/v1/realtime.proto` is the source of truth (TD-04). This
//! crate compiles it at build time and exposes the generated types plus the
//! version negotiation constants. Conversions between wire types and domain
//! types stay in the adapters that own them (ADR-004).

pub mod conversion;
pub mod version;

pub use conversion::{parse_optional_instance_id, render_optional_instance_id};
pub use version::{NegotiationError, SUPPORTED_MINOR_MAX, SUPPORTED_MINOR_MIN, negotiate_minor};

/// Protocol Buffers types generated from `proto/orbisync/v1/realtime.proto`.
///
/// The module is generated into `OUT_DIR` on every build; do not hand edit
/// generated code (specification §30.3).
#[allow(missing_docs, clippy::all, clippy::pedantic, unused_qualifications)]
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/orbisync.v1.rs"));
}

/// Major protocol version implemented by this crate (ADR-004).
pub const PROTOCOL_MAJOR: u32 = 1;

/// WebSocket subprotocol advertised for the binary contract (ADR-001).
pub const WEBSOCKET_SUBPROTOCOL: &str = "orbisync.v1.protobuf";

/// WebSocket subprotocol reserved for the development-only JSON debug mode
/// (ADR-001, TB-02). It is disabled unless explicitly enabled by configuration.
pub const WEBSOCKET_SUBPROTOCOL_JSON_DEBUG: &str = "orbisync.v1.json";
