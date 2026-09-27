//! World definitions and instance lifecycle coordination.
//!
//! Owns world definitions, instance creation, listing and lifecycle
//! arbitration, and publishes the instance locator other modules resolve
//! against (`architecture.md` §3).
//!
//! # Dependency rule
//!
//! `world-directory` depends on `domain` and `application` only
//! (`repo-crate-conventions.md` §3.2).
//!
//! Milestone 1 implements the REST-facing use cases; Milestone 0 fixes the
//! locator port so adapters can be wired and faked.

use orbisync_application::ApplicationError;
use orbisync_domain::{InstanceId, WorldId};

/// Where an instance is currently hosted.
///
/// v1 runs a single server process (ADR-009), so the location is either local
/// or absent. The type exists so the call sites stay stable when a future
/// version distributes instances.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InstanceLocation {
    /// Instance that was resolved.
    pub instance_id: InstanceId,
    /// World the instance belongs to.
    pub world_id: WorldId,
}

/// Resolves an instance to the process that hosts it.
#[async_trait::async_trait]
pub trait InstanceLocator: Send + Sync + 'static {
    /// Returns the location of a running instance, or `None` when the instance
    /// is unknown or stopped.
    ///
    /// # Errors
    ///
    /// Returns an [`ApplicationError`] when the directory cannot be consulted.
    async fn locate(
        &self,
        instance_id: InstanceId,
    ) -> Result<Option<InstanceLocation>, ApplicationError>;
}
