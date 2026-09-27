//! Composition-root adapters for the extension application read ports.
use orbisync_application::extension_command::ExtensionReadPort;
use orbisync_application::{ApplicationError, AuditQueryPort};
use orbisync_domain::{EntityId, InstanceId};
use orbisync_world_runtime::RuntimeRegistry;
use serde_json::{Value, json};
use std::sync::Arc;
use uuid::Uuid;

/// Supplies bounded reads without giving extensions runtime or repository handles.
pub struct ExtensionReads {
    /// Shared registry; only the owning actor task reads an entity.
    pub registry: Arc<RuntimeRegistry>,
    /// Existing application audit query projection.
    pub audit: Arc<dyn AuditQueryPort>,
}

#[async_trait::async_trait]
impl ExtensionReadPort for ExtensionReads {
    async fn entity(
        &self,
        instance_id: InstanceId,
        entity_id: EntityId,
    ) -> Result<Option<Value>, ApplicationError> {
        let Some(handle) = self.registry.handle(instance_id) else {
            return Ok(None);
        };
        let entity = handle
            .read_entity_direct(entity_id)
            .await
            .map_err(ApplicationError::port_failure)?;
        Ok(entity.map(|entity| json!({
            "entity_id": entity.id().to_string(), "instance_id": entity.instance_id().to_string(),
            "kind": entity.kind().as_str(), "owner_user_id": entity.owner().map(|id| id.to_string()),
            "revision": entity.revision().as_u64().to_string(), "components": entity.components(),
            "transform": entity.transform().map(|t| json!({
                "position": {"x":t.position().x(),"y":t.position().y(),"z":t.position().z()},
                "rotation": {"x":t.rotation().x(),"y":t.rotation().y(),"z":t.rotation().z(),"w":t.rotation().w()},
                "scale": {"x":t.scale().x(),"y":t.scale().y(),"z":t.scale().z()}
            }))
        })))
    }
    async fn audit(&self, event_id: Uuid) -> Result<Option<Value>, ApplicationError> {
        let event = self
            .audit
            .event(event_id)
            .await
            .map_err(|_| ApplicationError::port_failure("audit query unavailable"))?;
        Ok(event.map(|event| json!({ "audit_id":event.id, "timestamp":event.occurred_at.as_offset_date_time().format(&time::format_description::well_known::Rfc3339).unwrap_or_default(),
            "actor_type":event.actor_type, "actor_id":event.actor_id.map(|id|id.to_string()),
            "action":event.action, "resource_type":event.resource_type, "resource_id":event.resource_id,
            "result":event.result, "error_code":event.error_code, "request_id":event.request_id, "details":event.details })))
    }
}
