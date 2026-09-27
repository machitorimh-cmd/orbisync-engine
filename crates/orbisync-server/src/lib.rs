//! Server composition root library exports.
//!
//! The binary `orbisync-server` (`main.rs`) is the composition root and holds
//! no business logic. This library crate owns the server modules
//! (`delivery`, `interest_filter`, `realtime_ws`) so integration tests can
//! route real TCP through `handle_socket` (`realtime-ws` E2E, W-9).

pub mod checkpoint_admission;
pub mod checkpoint_generation;
pub mod checkpoint_operator;
pub mod checkpoint_reconciliation;
pub mod command_dedup;
pub mod delivery;
pub mod extension_reads;
pub mod interest_filter;
pub mod realtime_ws;
pub mod runtime_tick;
pub mod shutdown;

pub mod input;
pub mod external_input;
