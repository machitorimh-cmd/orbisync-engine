//! Operator-owned, language-neutral input rule deployment. No client registration.
use std::{collections::HashSet, path::Path, sync::Arc};

use orbisync_domain::{CommandId, WorldId};
use orbisync_extensions::{
    PreCommitBodyClient, PreCommitValidationPolicy, SecretProvider, build_signed_webhook,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::input::{InputContext, InputRules, RegisteredInputRule};

/// Versioned startup manifest. Changes require a server restart.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputRuleManifest {
    /// Currently 1.
    pub version: u32,
    /// World-scoped bindings, never supplied by game clients.
    pub rules: Vec<ExternalInputBinding>,
}

/// A trusted operator grants one service authority to propose one component.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalInputBinding {
    /// Existing world definition UUID.
    #[serde(deserialize_with = "deserialize_world")]
    pub world_id: WorldId,
    /// Contract name passed to SDK sendInput.
    pub rule: String,
    /// Application component (core.* is forbidden).
    pub component_key: String,
    /// HTTPS URL subject to existing extension egress controls.
    pub endpoint: String,
    /// Environment secret reference, using the extension secret provider.
    pub signing_secret_ref: String,
}

fn deserialize_world<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<WorldId, D::Error> {
    String::deserialize(deserializer)?
        .parse()
        .map_err(serde::de::Error::custom)
}

/// Shared transport and bounded admission, using existing extension policy.
pub struct ExternalInputTransport {
    client: Arc<dyn PreCommitBodyClient>,
    secrets: Arc<dyn SecretProvider>,
    policy: PreCommitValidationPolicy,
    inflight: Semaphore,
}

impl ExternalInputTransport {
    /// Reuses the stock extension HTTPS client, CA/SSRF policy and secret provider.
    pub fn new(
        client: Arc<dyn PreCommitBodyClient>,
        secrets: Arc<dyn SecretProvider>,
        policy: PreCommitValidationPolicy,
    ) -> Self {
        Self {
            client,
            secrets,
            policy,
            inflight: Semaphore::new(policy.max_concurrency() as usize),
        }
    }
}

/// Registered external computation; only the engine commits its output.
#[derive(Clone)]
pub struct ExternalInputRule {
    /// Validated operator binding.
    pub binding: ExternalInputBinding,
    transport: Arc<ExternalInputTransport>,
}

impl InputRuleManifest {
    /// Loads the same manifest used by stock production composition.
    pub fn load(path: &Path) -> Result<Self, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("input rules file: {e}"))?;
        serde_json::from_slice(&bytes).map_err(|e| format!("input rules manifest: {e}"))
    }

    /// Validates all bindings and resolves secrets before serving any traffic.
    pub async fn register(
        self,
        transport: Arc<ExternalInputTransport>,
    ) -> Result<InputRules, String> {
        if self.version != 1 {
            return Err("unsupported input rules manifest version".into());
        }
        let mut rules = InputRules::new();
        let mut components = HashSet::new();
        for binding in self.rules {
            if binding.rule.is_empty()
                || binding.rule.len() > 256
                || binding.component_key.starts_with("core.")
                || binding.component_key.is_empty()
                || binding.component_key.len() > 128
                || !binding.component_key.contains('.')
                || binding.component_key.starts_with('.')
                || binding.component_key.ends_with('.')
                || !binding
                    .component_key
                    .bytes()
                    .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b".-_".contains(&b))
            {
                return Err("invalid input rule or component name".into());
            }
            let key = (binding.world_id, binding.rule.clone());
            if rules.contains_key(&key)
                || !components.insert((binding.world_id, binding.component_key.clone()))
            {
                return Err("duplicate input rule or component binding".into());
            }
            let secret = transport
                .secrets
                .resolve(&binding.signing_secret_ref)
                .await
                .map_err(|_| "input rule signing secret unavailable")?;
            if secret.is_empty() {
                return Err("input rule signing secret is empty".into());
            }
            build_signed_webhook(
                &binding.endpoint,
                uuid::Uuid::nil(),
                "input.compute",
                0,
                &Value::Null,
                &secret,
            )
            .map_err(|_| "invalid input rule HTTPS endpoint")?;
            rules.insert(
                key,
                RegisteredInputRule::External(ExternalInputRule {
                    binding,
                    transport: Arc::clone(&transport),
                }),
            );
        }
        Ok(rules)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleResponse {
    version: u32,
    request_id: String,
    decision: String,
    #[serde(default)]
    update: Option<Value>,
    #[serde(default)]
    reason: Option<String>,
}

impl ExternalInputRule {
    pub(crate) async fn compute(
        &self,
        context: InputContext<'_>,
        intent: &prost_types::Struct,
        command_id: CommandId,
    ) -> Result<prost_types::Struct, String> {
        let transport = &self.transport;
        let _permit = transport
            .inflight
            .try_acquire()
            .map_err(|_| "input rule concurrency exhausted")?;
        let components = context.entity.components().iter().filter(|(key, _)| !key.starts_with("core."))
            .map(|(key, bytes)| {
                let value = match serde_json::from_slice::<Value>(bytes) {
                    Ok(value) => json!({"encoding": "json", "value": value}),
                    Err(_) => json!({"encoding": "base64", "value": base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes)}),
                };
                (key.clone(), value)
            }).collect::<serde_json::Map<_, _>>();
        let request_id = command_id.to_string();
        let now_unix_ms = context
            .now
            .to_unix_millis()
            .map_err(|_| "input clock unavailable")?;
        let payload = json!({
            "version": 1, "request_id": request_id,
            "world_id": context.world_id.to_string(), "instance_id": context.instance_id.to_string(),
            "requester": context.user_id.to_string(), "rule": self.binding.rule,
            "component_key": self.binding.component_key,
            "now_unix_ms": now_unix_ms.to_string(),
            "current_entity": {"entity_id": context.entity.id().to_string(),
                "revision": context.entity.revision().as_u64().to_string(),
                "owner_id": context.entity.owner().map(|id| id.to_string()), "components": components},
            "intent": crate::realtime_ws::struct_to_sorted_json_value(intent),
        });
        let response = tokio::time::timeout(transport.policy.timeout(), async {
            let secret = transport
                .secrets
                .resolve(&self.binding.signing_secret_ref)
                .await
                .map_err(|_| "input rule signing secret unavailable")?;
            let request = build_signed_webhook(
                &self.binding.endpoint,
                command_id.as_uuid(),
                "input.compute",
                time::OffsetDateTime::now_utc().unix_timestamp(),
                &payload,
                &secret,
            )
            .map_err(|_| "invalid input rule endpoint")?;
            transport
                .client
                .post(request, transport.policy.timeout())
                .await
                .map_err(|_| "input rule transport unavailable")
        })
        .await
        .map_err(|_| "input rule timed out")??;
        if !(200..300).contains(&response.status) {
            return Err("input rule returned non-2xx status".into());
        }
        let body: RuleResponse =
            serde_json::from_slice(&response.body).map_err(|_| "malformed input rule response")?;
        if body.version != 1 || body.request_id != request_id {
            return Err("input rule response version/correlation mismatch".into());
        }
        match body.decision.as_str() {
            "reject" if body.update.is_none() => {
                Err(body.reason.unwrap_or_else(|| "input rejected".into()))
            }
            "accept" if body.reason.is_none() => {
                let update = body.update.ok_or("input rule update missing")?;
                if !update.is_object()
                    || !crate::realtime_ws::json_fits_struct(&update)
                    || update.get("component_key").is_some()
                    || update.get("key").is_some()
                {
                    return Err("input rule update must be an object without component selectors and with safe JSON numbers".into());
                }
                match crate::realtime_ws::serde_json_to_prost_value(update).and_then(|v| v.kind) {
                    Some(prost_types::value::Kind::StructValue(value)) => Ok(value),
                    _ => Err("invalid input rule update".into()),
                }
            }
            _ => Err("invalid input rule decision".into()),
        }
    }
}
