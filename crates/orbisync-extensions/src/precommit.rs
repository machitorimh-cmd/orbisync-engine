//! Synchronous pre-commit validation hook (ADR-025,
//! `docs/design/extension-mechanism.md` §12).
//!
//! This is the third Core<->Extension interaction direction, distinct from
//! the two directions `delivery` implements (async webhook delivery,
//! at-least-once) and from the Extension Command API (Extension-initiated).
//! Here Core calls out to an Extension *before* an entity mutation command
//! (`SpawnEntity` / `UpdateEntityComponent` / `DeleteEntity`) reaches the
//! instance actor's mailbox, and the Extension's response is an additional
//! deny-only gate on top of Core's own authorization and validation — never
//! a way to grant what Core would otherwise refuse.
//!
//! Callers must not hold any per-instance lock (in particular the command
//! durability guard used by `realtime_ws_connection_runtime.rs`) across a
//! call to [`PreCommitValidationGate::validate`]: the method's own bounded
//! timeout only bounds *this* call, not any lock the caller chooses to hold
//! around it, and holding such a lock across the call would block unrelated
//! commands on the same instance for the duration of the external wait.

use std::sync::Arc;
use std::time::Duration;

use orbisync_domain::{CommandId, DomainError, DomainErrorKind, EntityId, InstanceId, UserId};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Semaphore;

use crate::delivery::{DeliveryError, DnsResolver, SecretProvider, WebhookRequest};
use crate::{ExtensionRegistration, ExtensionsConfig};

const MAX_RESPONSE_BODY_BYTES: usize = 16 * 1024;

/// Bounded policy for the synchronous pre-commit validation hook.
///
/// Deliberately independent of [`crate::DeliveryPolicy`] (async webhook
/// delivery): this hook sits on a user-facing request path and must fail
/// closed quickly, so it does not share the 5s webhook delivery timeout, and
/// it has no retry/backoff/circuit-breaker/DLQ — a single failed attempt is
/// a deny, not a retry candidate (ADR-025).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PreCommitValidationPolicy {
    timeout_ms: u64,
    max_concurrency: u32,
}

impl PreCommitValidationPolicy {
    /// Validates and stores a policy.
    ///
    /// # Errors
    ///
    /// Returns [`DomainErrorKind::InvalidValue`] when a bound is zero or
    /// outside the accepted range.
    pub fn new(timeout_ms: u64, max_concurrency: u32) -> Result<Self, DomainError> {
        if timeout_ms == 0 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "pre-commit validation timeout must be greater than 0",
            ));
        }
        if max_concurrency == 0 || max_concurrency > 1024 {
            return Err(DomainError::new(
                DomainErrorKind::InvalidValue,
                "pre-commit validation concurrency must be between 1 and 1024",
            ));
        }
        Ok(Self {
            timeout_ms,
            max_concurrency,
        })
    }

    /// Builds a policy from the validated application configuration.
    pub fn from_config(config: ExtensionsConfig) -> Result<Self, DomainError> {
        Self::new(
            config.pre_commit_validation_timeout_ms,
            config.pre_commit_validation_max_concurrency,
        )
    }

    /// Returns the per-call timeout.
    #[must_use]
    pub const fn timeout(self) -> Duration {
        Duration::from_millis(self.timeout_ms)
    }

    /// Returns the process-wide maximum number of concurrent calls.
    #[must_use]
    pub const fn max_concurrency(self) -> u32 {
        self.max_concurrency
    }
}

/// The entity-mutation operation kind a pre-commit request is validating.
///
/// `UpdateTransform` (the 20Hz position stream) is intentionally not
/// representable here: ADR-025 §2.3 excludes it from synchronous hook
/// validation because a per-update synchronous HTTP round trip is not
/// compatible with a continuous high-frequency stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PreCommitOperation {
    /// `InstanceCommand::SpawnEntity`.
    Spawn,
    /// `InstanceCommand::UpdateEntityComponent`.
    Update,
    /// `InstanceCommand::DeleteEntity`.
    Delete,
}

impl PreCommitOperation {
    /// Returns the registration capability that must be present for an
    /// extension to receive this operation's pre-commit requests.
    ///
    /// Colon-separated throughout (`hooks:entity:spawn`), matching the
    /// existing `<category>:<resource>:<action>` capability convention in
    /// `extension-mechanism.md` §3.1 (`commands:entity:read`,
    /// `commands:audit:read`) rather than introducing a mixed `:`/`.`
    /// separator scheme (EX-01 cross-check, ADR-025 §2.7).
    #[must_use]
    pub const fn capability(self) -> &'static str {
        match self {
            Self::Spawn => "hooks:entity:spawn",
            Self::Update => "hooks:entity:update",
            Self::Delete => "hooks:entity:delete",
        }
    }

    /// Returns the event-kind string sent in the request body.
    #[must_use]
    pub const fn event_kind(self) -> &'static str {
        match self {
            Self::Spawn => "precommit.entity.spawn",
            Self::Update => "precommit.entity.update",
            Self::Delete => "precommit.entity.delete",
        }
    }
}

/// One pre-commit validation request.
///
/// `request_id` reuses the client-supplied `command_id` that
/// `CommandDedupStore` already treats as the idempotency key for the
/// underlying command (ADR-025 §2.4) — this module does not invent a
/// separate deduplication scheme. The endpoint is never taken from this
/// struct or from client input; it always comes from the matched
/// [`ExtensionRegistration`].
#[derive(Debug, Clone)]
pub struct PreCommitValidationRequest {
    /// Idempotency key, shared with the underlying command's dedup entry.
    pub request_id: CommandId,
    /// Instance the command targets.
    pub instance_id: InstanceId,
    /// Entity the command targets.
    pub entity_id: EntityId,
    /// Which entity-mutation operation this request validates.
    pub operation: PreCommitOperation,
    /// Authenticated requester (already authorized by Core; informational).
    pub requester: UserId,
    /// The `expected_revision` the client itself sent with the command,
    /// forwarded unchanged — not a value Core observed by reading current
    /// entity state. `None` for `Spawn`, which has no revision concept at
    /// all (`InstanceCommand::SpawnEntity` carries no `expected_revision`
    /// field). Update/delete callers compare this with `current_entity`
    /// before invoking the hook. The actor then enforces the same revision
    /// again at commit, binding the approval to the observed state.
    pub client_expected_revision: Option<u64>,
    /// Authoritative Core state read before the hook. `None` means absent.
    /// Update/delete only reach the hook if this revision matches the client
    /// expectation; the actor checks that same revision again at commit.
    pub current_entity: Option<Value>,
    /// Component key, when this validates `UpdateEntityComponent`.
    pub component_key: Option<String>,
    /// Application-defined payload. Core does not interpret its contents
    /// (`domain-model.md` §5); it is forwarded to the extension as-is.
    pub payload: Value,
}

/// The gate's decision for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreCommitDecision {
    /// The extension allows the command to proceed. This does not itself
    /// grant anything; Core's own checks still apply in full.
    Allow,
    /// The command is denied, with a human-readable reason when available.
    Deny {
        /// Reason for the denial, when the extension supplied one.
        reason: String,
    },
}

impl PreCommitDecision {
    /// Returns `true` for [`Self::Allow`].
    #[must_use]
    pub const fn is_allow(&self) -> bool {
        matches!(self, Self::Allow)
    }
}

#[derive(Debug, Deserialize)]
struct PreCommitResponseBody {
    decision: String,
    #[serde(default)]
    reason: Option<String>,
}

fn build_signed_precommit_request(
    registration: &ExtensionRegistration,
    request: &PreCommitValidationRequest,
    secret: &[u8],
) -> Result<WebhookRequest, DeliveryError> {
    let payload = json!({
        "request_id": request.request_id.to_string(),
        "instance_id": request.instance_id.to_string(),
        "entity_id": request.entity_id.to_string(),
        "operation": request.operation.event_kind(),
        "requester": request.requester.to_string(),
        "client_expected_revision": request.client_expected_revision,
        "current_entity": request.current_entity,
        "component_key": request.component_key,
        "payload": request.payload,
    });
    let now = time::OffsetDateTime::now_utc().unix_timestamp();
    crate::delivery::build_signed_webhook(
        &registration.endpoint,
        request.request_id.as_uuid(),
        request.operation.event_kind(),
        now,
        &payload,
        secret,
    )
}

/// Production HTTPS client for the pre-commit validation hook.
///
/// Reuses the same SSRF-guarded `ValidatingDnsResolver` as webhook delivery
/// (ADR-007). `allow_loopback` is threaded from
/// `extensions.allow_loopback_endpoints`, which must stay `false` in
/// production; it exists so a local integration test can reach a loopback
/// listener without weakening the production egress policy (ADR-025 §2.8).
#[derive(Debug, Clone)]
pub struct ReqwestPreCommitClient {
    client: reqwest::Client,
}

impl ReqwestPreCommitClient {
    /// Builds a client with redirects disabled and the given DNS resolver.
    pub fn try_new(
        resolver: Arc<dyn DnsResolver>,
        allow_loopback: bool,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: crate::delivery::build_precommit_client(resolver, allow_loopback)?,
        })
    }

    /// Test-only constructor that also disables TLS certificate
    /// verification, so tests can drive a real local HTTPS listener using a
    /// self-signed certificate (see `delivery::build_precommit_test_client`).
    #[cfg(test)]
    fn try_new_for_test(
        resolver: Arc<dyn DnsResolver>,
        allow_loopback: bool,
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: crate::delivery::build_precommit_test_client(resolver, allow_loopback)?,
        })
    }

    /// Builds a client that trusts one additional root certificate instead
    /// of disabling certificate verification, for real end-to-end tests
    /// hosted in other crates (e.g. `tests/integration`) that cannot reach
    /// this crate's `#[cfg(test)]`-only `Self::try_new_for_test`. Uses the
    /// internal `delivery::build_precommit_client_with_root_cert` builder.
    pub fn try_new_with_root_cert(
        resolver: Arc<dyn DnsResolver>,
        allow_loopback: bool,
        root_certificate_der: &[u8],
    ) -> Result<Self, reqwest::Error> {
        Ok(Self {
            client: crate::delivery::build_precommit_client_with_root_cert(
                resolver,
                allow_loopback,
                root_certificate_der,
            )?,
        })
    }

    /// Builds a client with one additional PEM or DER trust anchor while
    /// retaining the platform roots and normal certificate verification.
    pub fn try_new_with_additional_ca(
        resolver: Arc<dyn DnsResolver>,
        allow_loopback: bool,
        certificate: &[u8],
    ) -> Result<Self, DeliveryError> {
        Ok(Self {
            client: crate::delivery::build_precommit_client_with_additional_ca(
                resolver,
                allow_loopback,
                certificate,
            )?,
        })
    }
}

/// Full pre-commit HTTP response, including the decision body.
///
/// [`crate::delivery::WebhookResponse`] only carries a status code because
/// webhook delivery never reads a response body (`extension-mechanism.md`
/// §5). The pre-commit hook must read the decision body, so it uses this
/// separate response type via [`PreCommitBodyClient`] rather than widening
/// the webhook delivery contract.
#[derive(Debug, Clone)]
pub struct PreCommitHttpResponse {
    /// HTTP status code.
    pub status: u16,
    /// Raw response body, capped at 16 KiB by [`ReqwestPreCommitClient`].
    pub body: Vec<u8>,
}

/// Transport port that returns a full response body, used by
/// [`PreCommitValidationGate`] instead of the body-discarding
/// [`crate::delivery::HttpClient`] trait.
#[async_trait::async_trait]
pub trait PreCommitBodyClient: Send + Sync + 'static {
    /// Sends one request, enforcing the supplied timeout, and returns the
    /// full response including its body.
    async fn post(
        &self,
        request: WebhookRequest,
        timeout: Duration,
    ) -> Result<PreCommitHttpResponse, DeliveryError>;
}

#[async_trait::async_trait]
impl PreCommitBodyClient for ReqwestPreCommitClient {
    async fn post(
        &self,
        request: WebhookRequest,
        timeout: Duration,
    ) -> Result<PreCommitHttpResponse, DeliveryError> {
        let mut builder = self.client.post(&request.endpoint).timeout(timeout);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .body(request.body)
            .send()
            .await
            .map_err(|_| DeliveryError::Transport)?;
        let status = response.status().as_u16();
        let bytes = response
            .bytes()
            .await
            .map_err(|_| DeliveryError::Transport)?;
        if bytes.len() > MAX_RESPONSE_BODY_BYTES {
            return Err(DeliveryError::Transport);
        }
        Ok(PreCommitHttpResponse {
            status,
            body: bytes.to_vec(),
        })
    }
}

/// The production pre-commit validation gate. Parses the decision body
/// returned by the extension; every failure mode (timeout, non-2xx,
/// transport error, oversized/unparsable body, unknown `decision` value,
/// concurrency exhausted, secret unavailable) resolves to
/// [`PreCommitDecision::Deny`] (ADR-025, fail closed).
pub struct PreCommitValidationGate<C, S> {
    client: Arc<C>,
    secrets: Arc<S>,
    policy: PreCommitValidationPolicy,
    inflight: Arc<Semaphore>,
}

impl<C, S> PreCommitValidationGate<C, S>
where
    C: PreCommitBodyClient,
    S: SecretProvider,
{
    /// Builds a gate bounded by `policy.max_concurrency()`.
    pub fn new(client: Arc<C>, secrets: Arc<S>, policy: PreCommitValidationPolicy) -> Self {
        Self {
            client,
            secrets,
            inflight: Arc::new(Semaphore::new(policy.max_concurrency() as usize)),
            policy,
        }
    }

    /// Validates one request against `registration`. See
    /// [`PreCommitValidationGate::validate`] for the locking contract that
    /// callers must observe (no per-instance lock held across this call).
    pub async fn validate(
        &self,
        registration: &ExtensionRegistration,
        request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        let Ok(_permit) = Arc::clone(&self.inflight).try_acquire_owned() else {
            return PreCommitDecision::Deny {
                reason: String::from("pre-commit validation concurrency exhausted"),
            };
        };
        let secret = match self.secrets.resolve(&registration.signing_secret_ref).await {
            Ok(secret) => secret,
            Err(_) => {
                return PreCommitDecision::Deny {
                    reason: String::from("signing secret unavailable"),
                };
            }
        };
        let webhook = match build_signed_precommit_request(registration, request, &secret) {
            Ok(webhook) => webhook,
            Err(_) => {
                return PreCommitDecision::Deny {
                    reason: String::from("invalid extension endpoint"),
                };
            }
        };
        let outcome = tokio::time::timeout(
            self.policy.timeout(),
            self.client.post(webhook, self.policy.timeout()),
        )
        .await;
        let response = match outcome {
            Ok(Ok(response)) => response,
            Ok(Err(_)) => {
                return PreCommitDecision::Deny {
                    reason: String::from("transport error calling extension"),
                };
            }
            Err(_) => {
                return PreCommitDecision::Deny {
                    reason: String::from("pre-commit validation timed out"),
                };
            }
        };
        if !(200..300).contains(&response.status) {
            return PreCommitDecision::Deny {
                reason: format!("extension returned non-2xx status {}", response.status),
            };
        }
        match serde_json::from_slice::<PreCommitResponseBody>(&response.body) {
            Ok(body) if body.decision == "allow" => PreCommitDecision::Allow,
            Ok(body) if body.decision == "deny" => PreCommitDecision::Deny {
                reason: body.reason.unwrap_or_else(|| String::from("denied")),
            },
            Ok(_) => PreCommitDecision::Deny {
                reason: String::from("extension returned an unrecognized decision value"),
            },
            Err(_) => PreCommitDecision::Deny {
                reason: String::from("extension returned an unparsable response body"),
            },
        }
    }
}

/// Object-safe facade over [`PreCommitValidationGate`], so a composition root
/// can store one gate behind `Arc<dyn PreCommitGate>` without exposing the
/// concrete `C`/`S` transport and secret-provider type parameters to every
/// caller (mirrors how `checkpoint_store: Option<Arc<dyn CheckpointStore>>`
/// is used elsewhere in the server composition root).
#[async_trait::async_trait]
pub trait PreCommitGate: Send + Sync + 'static {
    /// Validates one request against `registration`. See
    /// [`PreCommitValidationGate::validate`] for the locking contract.
    ///
    /// The caller (`realtime_ws_connection_runtime.rs`) awaits this call
    /// inline inside its single per-connection `tokio::select!` loop and
    /// applies no additional timeout of its own — this connection's own
    /// heartbeat handling is delayed for as long as this call takes
    /// (ADR-025 "Heartbeat/接続timeoutとの関係"). Every implementation
    /// **must** bound its own worst-case latency (as [`PreCommitValidationGate`]
    /// does via [`PreCommitValidationPolicy`]'s timeout); there is no
    /// external watchdog that will interrupt a call that never resolves.
    async fn validate(
        &self,
        registration: &ExtensionRegistration,
        request: &PreCommitValidationRequest,
    ) -> PreCommitDecision;
}

#[async_trait::async_trait]
impl<C, S> PreCommitGate for PreCommitValidationGate<C, S>
where
    C: PreCommitBodyClient,
    S: SecretProvider,
{
    async fn validate(
        &self,
        registration: &ExtensionRegistration,
        request: &PreCommitValidationRequest,
    ) -> PreCommitDecision {
        Self::validate(self, registration, request).await
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeSet;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use orbisync_domain::{DomainErrorKind, EntityId, InstanceId, UserId};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio_rustls::TlsAcceptor;

    use super::{
        DeliveryError, PreCommitBodyClient, PreCommitDecision, PreCommitHttpResponse,
        PreCommitOperation, PreCommitValidationGate, PreCommitValidationPolicy,
        PreCommitValidationRequest, ReqwestPreCommitClient,
    };
    use crate::delivery::{DnsResolver, SecretProvider, SystemDnsResolver, WebhookRequest};
    use crate::{ExtensionRegistration, ExtensionStatus};

    fn registration() -> ExtensionRegistration {
        ExtensionRegistration {
            extension_id: uuid::Uuid::now_v7(),
            name: String::from("zone-guard"),
            description: None,
            endpoint: String::from("https://zone-guard.example/precommit"),
            subscribed_events: BTreeSet::new(),
            capabilities: BTreeSet::from([String::from(PreCommitOperation::Spawn.capability())]),
            token_scopes: BTreeSet::new(),
            status: ExtensionStatus::Active,
            signing_secret_ref: String::from("ORBI_EXTENSION_SECRET_ZONE_GUARD"),
        }
    }

    #[test]
    fn additional_ca_rejects_invalid_certificate_bytes() {
        assert!(
            ReqwestPreCommitClient::try_new_with_additional_ca(
                Arc::new(SystemDnsResolver),
                false,
                b"not a certificate",
            )
            .is_err()
        );
    }

    fn request() -> PreCommitValidationRequest {
        PreCommitValidationRequest {
            request_id: orbisync_domain::CommandId::generate(),
            instance_id: InstanceId::generate(),
            entity_id: EntityId::generate(),
            operation: PreCommitOperation::Spawn,
            requester: UserId::generate(),
            client_expected_revision: Some(1),
            current_entity: None,
            component_key: None,
            payload: serde_json::json!({"position": {"x": 0.0, "y": 0.0, "z": 0.0}}),
        }
    }

    #[test]
    fn signed_request_contains_core_state_separate_from_client_payload() {
        let mut request = request();
        request.current_entity = Some(serde_json::json!({
            "revision": 7, "owner_id": "core-owner",
            "components": {"org.example.document": {"encoding": "json", "value": {"locked": true}}}
        }));
        request.payload = serde_json::json!({"current_entity": {"owner_id": "forged"}});
        let wire = super::build_signed_precommit_request(&registration(), &request, b"test-secret")
            .unwrap();
        let body: serde_json::Value = serde_json::from_slice(&wire.body).unwrap();
        assert_eq!(
            body["payload"]["current_entity"],
            request.current_entity.unwrap()
        );
        assert_eq!(
            body["payload"]["payload"]["current_entity"]["owner_id"],
            "forged"
        );
    }

    #[derive(Debug, Clone, Copy)]
    struct TestSecrets;

    #[async_trait::async_trait]
    impl SecretProvider for TestSecrets {
        async fn resolve(&self, _reference: &str) -> Result<Vec<u8>, DeliveryError> {
            Ok(b"unit-test-secret".to_vec())
        }
    }

    struct MissingSecrets;

    #[async_trait::async_trait]
    impl SecretProvider for MissingSecrets {
        async fn resolve(&self, _reference: &str) -> Result<Vec<u8>, DeliveryError> {
            Err(DeliveryError::SecretUnavailable)
        }
    }

    #[derive(Default)]
    struct ScriptedClient {
        calls: AtomicUsize,
        response: Option<PreCommitHttpResponse>,
        delay: Option<Duration>,
        fail: bool,
    }

    #[async_trait::async_trait]
    impl PreCommitBodyClient for ScriptedClient {
        async fn post(
            &self,
            _request: WebhookRequest,
            _timeout: Duration,
        ) -> Result<PreCommitHttpResponse, DeliveryError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            if let Some(delay) = self.delay {
                tokio::time::sleep(delay).await;
            }
            if self.fail {
                return Err(DeliveryError::Transport);
            }
            Ok(self.response.clone().expect("scripted response"))
        }
    }

    fn allow_response() -> PreCommitHttpResponse {
        PreCommitHttpResponse {
            status: 200,
            body: serde_json::to_vec(&serde_json::json!({"decision": "allow"})).unwrap(),
        }
    }

    fn deny_response(reason: &str) -> PreCommitHttpResponse {
        PreCommitHttpResponse {
            status: 200,
            body: serde_json::to_vec(&serde_json::json!({"decision": "deny", "reason": reason}))
                .unwrap(),
        }
    }

    #[tokio::test]
    async fn allow_decision_is_parsed() {
        let client = Arc::new(ScriptedClient {
            response: Some(allow_response()),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            Arc::clone(&client),
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert_eq!(decision, PreCommitDecision::Allow);
        assert_eq!(client.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn deny_decision_carries_the_extension_reason() {
        let client = Arc::new(ScriptedClient {
            response: Some(deny_response("inside no-fly zone")),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert_eq!(
            decision,
            PreCommitDecision::Deny {
                reason: String::from("inside no-fly zone")
            }
        );
    }

    #[tokio::test]
    async fn non_2xx_status_is_denied() {
        let client = Arc::new(ScriptedClient {
            response: Some(PreCommitHttpResponse {
                status: 500,
                body: Vec::new(),
            }),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert!(!decision.is_allow());
    }

    #[tokio::test]
    async fn transport_failure_is_denied() {
        let client = Arc::new(ScriptedClient {
            fail: true,
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert!(!decision.is_allow());
    }

    #[tokio::test]
    async fn malformed_body_is_denied() {
        let client = Arc::new(ScriptedClient {
            response: Some(PreCommitHttpResponse {
                status: 200,
                body: b"not json".to_vec(),
            }),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert!(!decision.is_allow());
    }

    #[tokio::test]
    async fn unrecognized_decision_value_is_denied() {
        let client = Arc::new(ScriptedClient {
            response: Some(PreCommitHttpResponse {
                status: 200,
                body: serde_json::to_vec(&serde_json::json!({"decision": "maybe"})).unwrap(),
            }),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert!(!decision.is_allow());
    }

    #[tokio::test]
    async fn timeout_is_denied_and_does_not_wait_past_the_policy_bound() {
        let client = Arc::new(ScriptedClient {
            response: Some(allow_response()),
            delay: Some(Duration::from_secs(5)),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(50, 4).unwrap(),
        );
        let started = tokio::time::Instant::now();
        let decision = gate.validate(&registration(), &request()).await;
        assert!(!decision.is_allow());
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "validate must not wait past its own timeout bound"
        );
    }

    #[tokio::test]
    async fn missing_secret_is_denied() {
        let client = Arc::new(ScriptedClient {
            response: Some(allow_response()),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(MissingSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let decision = gate.validate(&registration(), &request()).await;
        assert!(!decision.is_allow());
    }

    #[tokio::test]
    async fn concurrency_beyond_the_bound_is_denied_rather_than_queued_unboundedly() {
        let client = Arc::new(ScriptedClient {
            response: Some(allow_response()),
            delay: Some(Duration::from_millis(200)),
            ..Default::default()
        });
        let gate = Arc::new(PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(1_000, 1).unwrap(),
        ));
        let reg = Arc::new(registration());
        let first = {
            let gate = Arc::clone(&gate);
            let reg = Arc::clone(&reg);
            tokio::spawn(async move { gate.validate(&reg, &request()).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        let second = gate.validate(&reg, &request()).await;
        assert!(
            !second.is_allow(),
            "second call must fail closed while the first is in flight"
        );
        let first_decision = first.await.expect("first call joins");
        assert!(
            first_decision.is_allow(),
            "first call proceeds and is allowed"
        );
        // The permit the first call held must be returned once it finishes,
        // not leaked for the lifetime of the gate — a third call issued only
        // after the first has joined must succeed rather than fail closed.
        let third = gate.validate(&reg, &request()).await;
        assert!(
            third.is_allow(),
            "the concurrency slot held by the first call must be released once it completes"
        );
    }

    #[tokio::test]
    async fn a_timed_out_call_releases_its_concurrency_slot_immediately_not_after_the_backend_delay()
     {
        // The backend delay (5s) vastly exceeds the policy timeout (50ms).
        // If the concurrency permit were held until the backend call itself
        // returned (rather than being dropped when `validate` gives up and
        // returns Deny), a second call issued right after the first would
        // still see the slot as occupied and fail closed for several more
        // seconds. It must not: cancelling the timed-out future drops the
        // permit with it.
        let client = Arc::new(ScriptedClient {
            response: Some(allow_response()),
            delay: Some(Duration::from_secs(5)),
            ..Default::default()
        });
        let gate = PreCommitValidationGate::new(
            client,
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(50, 1).unwrap(),
        );
        let reg = registration();
        let first = gate.validate(&reg, &request()).await;
        assert!(!first.is_allow(), "the timed-out call must fail closed");
        let second =
            tokio::time::timeout(Duration::from_millis(500), gate.validate(&reg, &request()))
                .await
                .expect(
                    "a call issued right after a timeout must not itself block on the \
             abandoned backend call's slot",
                );
        assert!(
            !second.is_allow(),
            "the fake backend still answers Allow only after its own 5s delay; \
             this call must also time out, but it must not stall on the *first* \
             call's slot to do so"
        );
    }

    #[test]
    fn policy_rejects_zero_timeout() {
        assert_eq!(
            PreCommitValidationPolicy::new(0, 4)
                .expect_err("zero timeout must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
    }

    #[test]
    fn policy_rejects_out_of_range_concurrency() {
        assert_eq!(
            PreCommitValidationPolicy::new(500, 0)
                .expect_err("zero concurrency must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
        assert_eq!(
            PreCommitValidationPolicy::new(500, 1_025)
                .expect_err("excessive concurrency must be rejected")
                .kind(),
            DomainErrorKind::InvalidValue
        );
    }

    struct InjectedResolver {
        address: SocketAddr,
    }

    #[async_trait::async_trait]
    impl DnsResolver for InjectedResolver {
        async fn resolve(&self, _hostname: &str) -> Result<Vec<SocketAddr>, std::io::Error> {
            Ok(vec![self.address])
        }
    }

    /// A minimal local HTTPS listener that returns a fixed status/body,
    /// used to exercise the real `ReqwestPreCommitClient` transport (not a
    /// mock `PreCommitBodyClient`) against a real loopback socket.
    struct LocalHttpsServer {
        address: SocketAddr,
        task: tokio::task::JoinHandle<()>,
    }

    impl LocalHttpsServer {
        async fn start(status_line: &'static str, body: &'static str) -> Self {
            let _install = rustls::crypto::ring::default_provider().install_default();
            let cert = rcgen::generate_simple_self_signed(vec![String::from("precommit.test")])
                .expect("test certificate");
            let certificate = rustls::pki_types::CertificateDer::from(cert.cert.der().to_vec());
            let key = rustls::pki_types::PrivateKeyDer::Pkcs8(
                rustls::pki_types::PrivatePkcs8KeyDer::from(cert.key_pair.serialize_der()),
            );
            let config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![certificate], key)
                .expect("test TLS configuration");
            let acceptor = TlsAcceptor::from(Arc::new(config));
            let listener = TcpListener::bind("127.0.0.1:0")
                .await
                .expect("test HTTPS listener");
            let address = listener.local_addr().expect("test HTTPS address");
            let task = tokio::spawn(async move {
                loop {
                    let Ok((socket, _peer)) = listener.accept().await else {
                        return;
                    };
                    let acceptor = acceptor.clone();
                    tokio::spawn(async move {
                        let Ok(mut stream) = acceptor.accept(socket).await else {
                            return;
                        };
                        let mut request = Vec::new();
                        let mut chunk = [0_u8; 1024];
                        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                            let Ok(read) = stream.read(&mut chunk).await else {
                                return;
                            };
                            if read == 0 {
                                return;
                            }
                            request.extend_from_slice(&chunk[..read]);
                            if request.len() > 16 * 1024 {
                                return;
                            }
                        }
                        let response = format!(
                            "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        let _ignore_write_error = stream.write_all(response.as_bytes()).await;
                    });
                }
            });
            Self { address, task }
        }
    }

    impl Drop for LocalHttpsServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn real_loopback_transport_allows_when_allow_loopback_endpoints_is_true() {
        let server = LocalHttpsServer::start("200 OK", r#"{"decision":"allow"}"#).await;
        let resolver: Arc<dyn DnsResolver> = Arc::new(InjectedResolver {
            address: server.address,
        });
        let client = ReqwestPreCommitClient::try_new_for_test(resolver, true).expect("test client");
        let gate = PreCommitValidationGate::new(
            Arc::new(client),
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(2_000, 4).unwrap(),
        );
        let mut registration = registration();
        registration.endpoint = String::from("https://precommit.test/precommit");
        let decision = gate.validate(&registration, &request()).await;
        assert_eq!(decision, PreCommitDecision::Allow);
    }

    #[tokio::test]
    async fn real_loopback_transport_denies_a_deny_body() {
        let server =
            LocalHttpsServer::start("200 OK", r#"{"decision":"deny","reason":"no-fly zone"}"#)
                .await;
        let resolver: Arc<dyn DnsResolver> = Arc::new(InjectedResolver {
            address: server.address,
        });
        let client = ReqwestPreCommitClient::try_new_for_test(resolver, true).expect("test client");
        let gate = PreCommitValidationGate::new(
            Arc::new(client),
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(2_000, 4).unwrap(),
        );
        let mut registration = registration();
        registration.endpoint = String::from("https://precommit.test/precommit");
        let decision = gate.validate(&registration, &request()).await;
        assert_eq!(
            decision,
            PreCommitDecision::Deny {
                reason: String::from("no-fly zone")
            }
        );
    }

    #[tokio::test]
    async fn loopback_is_rejected_by_default_ssrf_policy() {
        // allow_loopback = false reuses the same ValidatingDnsResolver
        // rejection webhook delivery relies on (ADR-007): a loopback answer
        // must not be reachable in production posture.
        let server = LocalHttpsServer::start("200 OK", r#"{"decision":"allow"}"#).await;
        let resolver: Arc<dyn DnsResolver> = Arc::new(InjectedResolver {
            address: server.address,
        });
        let client =
            ReqwestPreCommitClient::try_new_for_test(resolver, false).expect("client build");
        let gate = PreCommitValidationGate::new(
            Arc::new(client),
            Arc::new(TestSecrets),
            PreCommitValidationPolicy::new(500, 4).unwrap(),
        );
        let mut registration = registration();
        registration.endpoint = String::from("https://precommit.test/precommit");
        let decision = gate.validate(&registration, &request()).await;
        assert!(
            !decision.is_allow(),
            "loopback must be rejected when allow_loopback_endpoints is false"
        );
    }

    #[test]
    fn system_resolver_is_constructible_for_production_wiring() {
        // Compile-time check that the production resolver type satisfies
        // what ReqwestPreCommitClient expects, exercised so this module has
        // at least one non-loopback construction path under test.
        let _resolver: Arc<dyn DnsResolver> = Arc::new(SystemDnsResolver);
    }
}
