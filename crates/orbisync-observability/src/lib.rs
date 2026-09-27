//! Telemetry facade.
//!
//! Other modules never build a tracing or metrics adapter themselves; they call
//! this facade (`observability-and-config.md` §1). Milestone 0 provides the
//! structured logging backend and the audit sink adapter; metrics and trace
//! exporters follow in later milestones.
//!
//! # Dependency rule
//!
//! `observability` depends on `domain`, `config` and the `application` ports it
//! implements (`repo-crate-conventions.md` §3.2).

pub mod audit;
pub mod logging;
pub mod metrics;

pub use audit::TracingAuditSink;
pub use logging::{ObservabilityError, init_logging};
pub use metrics::{NoopExporter, OperationalMetricsSnapshot, PrometheusMetrics};
