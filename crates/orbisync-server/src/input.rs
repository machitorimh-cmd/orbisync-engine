//! Trusted, application-owned input rules. No game schema is interpreted by Core.
use orbisync_domain::{Entity, InstanceId, Timestamp, UserId, WorldId};
use prost_types::Struct;
use std::{collections::HashMap, sync::Arc};

/// Immutable authoritative context. Rules must be pure, bounded computations:
/// no I/O or side effects; an optimistic conflict may discard their output.
pub struct InputContext<'a> {
    /// World selected by the authenticated connection.
    pub world_id: WorldId,
    /// Instance selected by the authenticated connection.
    pub instance_id: InstanceId,
    /// Authenticated requester (never supplied in intent).
    pub user_id: UserId,
    /// Exact entity state guarded at commit.
    pub entity: &'a Entity,
    /// Server clock time.
    pub now: Timestamp,
}

/// A rule computes one component update from intent and canonical entity state.
/// Return application data; the adapter adds the registered `component_key`.
/// Returning an error rejects the input without a mutation. Existing update
/// authorization, validation, precommit hooks and persistence still apply.
pub trait InputRule: Send + Sync + 'static {
    /// Component owned by this rule in its world. Direct client updates to this
    /// component are refused; other collaboration components remain writable.
    fn component_key(&self) -> &str;

    /// Compute an authoritative component update; never trust intent as an outcome.
    fn compute(&self, context: InputContext<'_>, intent: &Struct) -> Result<Struct, String>;
}

/// Registered only by trusted server composition, scoped by world and rule name.
pub type InputRules = HashMap<(WorldId, String), RegisteredInputRule>;

/// Local code or an operator-provisioned external service.
#[derive(Clone)]
pub enum RegisteredInputRule {
    /// Existing synchronous application rule.
    Local(Arc<dyn InputRule>),
    /// Language-neutral HTTP rule.
    External(crate::external_input::ExternalInputRule),
}

impl RegisteredInputRule {
    /// Component protected from direct client updates.
    pub fn component_key(&self) -> &str {
        match self {
            Self::Local(rule) => rule.component_key(),
            Self::External(rule) => &rule.binding.component_key,
        }
    }

    /// Computes a proposal without holding the actor/durability lock.
    pub async fn compute(
        &self,
        context: InputContext<'_>,
        intent: &Struct,
        command_id: orbisync_domain::CommandId,
    ) -> Result<Struct, String> {
        match self {
            Self::Local(rule) => rule.compute(context, intent),
            Self::External(rule) => rule.compute(context, intent, command_id).await,
        }
    }
}
