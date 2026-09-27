//! Signed webhook delivery with bounded retries and per-destination isolation.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::Mutex as StdMutex;
use std::time::Duration;

use futures_util::StreamExt;
use hmac::{Hmac, Mac};
use orbisync_application::metrics::{Counter, ExtensionDeliveryResult, Gauge, MetricsRecorder};
use orbisync_application::{ExtensionDeliveryStore, PendingExtensionDelivery};
use orbisync_domain::{Clock, SystemClock};
use reqwest::redirect::Policy;
use serde_json::{Value, json};
use sha2::Sha256;
use time::OffsetDateTime;
use tokio::sync::{Mutex, Semaphore};

use crate::{DeliveryPolicy, ExtensionRegistration};

type HmacSha256 = Hmac<Sha256>;

const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024;

/// An outbound signed webhook request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebhookRequest {
    /// Validated HTTPS endpoint.
    pub endpoint: String,
    /// Request headers. Secret material is never included in this structure.
    pub headers: Vec<(String, String)>,
    /// JSON request body.
    pub body: Vec<u8>,
}

/// The portion of an HTTP response needed by the delivery policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WebhookResponse {
    /// HTTP status code.
    pub status: u16,
}

/// Transport-independent HTTP client port.
#[async_trait::async_trait]
pub trait HttpClient: Send + Sync + 'static {
    /// Sends one request, enforcing the supplied timeout.
    async fn post(
        &self,
        request: WebhookRequest,
        timeout: Duration,
    ) -> Result<WebhookResponse, DeliveryError>;
}

/// Secret lookup port. Implementations must not log returned bytes.
#[async_trait::async_trait]
pub trait SecretProvider: Send + Sync + 'static {
    /// Resolves a secret-store reference into signing bytes.
    async fn resolve(&self, reference: &str) -> Result<Vec<u8>, DeliveryError>;
}

/// DNS lookup port used by webhook delivery.
///
/// Implementations must return the complete answer for one lookup. The
/// production implementation uses the system resolver; tests can inject a
/// deterministic answer (or a sequence of answers to model rebinding).
#[async_trait::async_trait]
pub trait DnsResolver: Send + Sync + 'static {
    /// Resolves one hostname to all of its socket addresses.
    async fn resolve(&self, hostname: &str) -> Result<Vec<SocketAddr>, std::io::Error>;
}

/// System DNS resolver used by the production webhook client.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemDnsResolver;

#[async_trait::async_trait]
impl DnsResolver for SystemDnsResolver {
    async fn resolve(&self, hostname: &str) -> Result<Vec<SocketAddr>, std::io::Error> {
        Ok(tokio::net::lookup_host((hostname, 443)).await?.collect())
    }
}

/// Resolves signing references from environment variables.
#[derive(Debug, Clone, Copy, Default)]
pub struct EnvSecretProvider;

#[async_trait::async_trait]
impl SecretProvider for EnvSecretProvider {
    async fn resolve(&self, reference: &str) -> Result<Vec<u8>, DeliveryError> {
        std::env::var(reference)
            .ok()
            .filter(|value| !value.is_empty())
            .map(String::into_bytes)
            .ok_or(DeliveryError::SecretUnavailable)
    }
}

/// Production HTTPS client. Redirects are disabled to prevent policy bypass.
#[derive(Debug, Clone)]
pub struct ReqwestHttpClient {
    client: ClientState,
}

impl ReqwestHttpClient {
    /// Creates a client with redirects disabled and system DNS resolution.
    ///
    /// This infallible constructor is retained for source compatibility. If
    /// reqwest cannot be built, the client retains a failed sentinel and every
    /// request returns the ordinary transport error instead of using a less
    /// restrictive fallback client. New composition roots should prefer
    /// [`Self::try_new`] so startup can report a build failure.
    #[must_use]
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(client) => client,
            Err(_) => Self::failed(),
        }
    }

    /// Creates a client with redirects disabled and system DNS resolution.
    pub fn try_new() -> Result<Self, reqwest::Error> {
        Self::with_resolver(Arc::new(SystemDnsResolver))
    }

    /// Creates a client with an injected DNS resolver.
    ///
    /// The resolver is wrapped in reqwest's documented resolver hook. Every
    /// answer is checked before reqwest receives it, so the addresses returned
    /// by DNS are the only addresses the connector can use. Idle connection
    /// pooling is disabled so a later delivery retry performs a fresh lookup
    /// and validation instead of reusing the previous attempt's connection.
    pub fn with_resolver(resolver: Arc<dyn DnsResolver>) -> Result<Self, reqwest::Error> {
        let client = build_client(reqwest::Client::builder(), resolver)?;
        Ok(Self {
            client: ClientState::Ready(client),
        })
    }

    fn failed() -> Self {
        Self {
            client: ClientState::BuildFailed,
        }
    }
}

impl Default for ReqwestHttpClient {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone)]
enum ClientState {
    Ready(reqwest::Client),
    BuildFailed,
}

fn build_client(
    builder: reqwest::ClientBuilder,
    resolver: Arc<dyn DnsResolver>,
) -> Result<reqwest::Client, reqwest::Error> {
    build_client_with_loopback_policy(builder, resolver, false)
}

#[cfg(test)]
pub(crate) fn build_test_client(
    resolver: Arc<dyn DnsResolver>,
) -> Result<reqwest::Client, reqwest::Error> {
    build_client_with_loopback_policy(
        reqwest::Client::builder().danger_accept_invalid_certs(true),
        resolver,
        true,
    )
}

/// Builds the underlying validated HTTPS client for the synchronous
/// pre-commit validation hook (`precommit` module, ADR-025). Shares the same
/// SSRF-guarded `ValidatingDnsResolver` as webhook delivery; `allow_loopback`
/// is threaded from `extensions.allow_loopback_endpoints`, which must stay
/// `false` in production (ADR-025 §2.8).
pub(crate) fn build_precommit_client(
    resolver: Arc<dyn DnsResolver>,
    allow_loopback: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    build_client_with_loopback_policy(reqwest::Client::builder(), resolver, allow_loopback)
}

/// Test-only variant of [`build_precommit_client`] that also disables TLS
/// certificate verification, so `precommit`'s tests can drive a real local
/// HTTPS listener using a self-signed certificate while still exercising the
/// real `allow_loopback` egress-policy branch (`ValidatingDnsResolver`).
#[cfg(test)]
pub(crate) fn build_precommit_test_client(
    resolver: Arc<dyn DnsResolver>,
    allow_loopback: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    build_client_with_loopback_policy(
        reqwest::Client::builder().danger_accept_invalid_certs(true),
        resolver,
        allow_loopback,
    )
}

/// Variant of [`build_precommit_client`] that trusts one additional root
/// certificate (DER-encoded) instead of disabling certificate verification.
///
/// This exists so a workspace-level integration test in a different crate
/// (which cannot see `#[cfg(test)]` items from this crate) can still drive a
/// genuine local HTTPS listener end to end with real certificate validation,
/// by presenting a certificate issued under a locally generated CA and
/// trusting only that CA here — unlike `build_precommit_test_client`, every
/// other certificate is still rejected exactly as in production
/// (ADR-025 §2.8, §2.9).
pub fn build_precommit_client_with_root_cert(
    resolver: Arc<dyn DnsResolver>,
    allow_loopback: bool,
    root_certificate_der: &[u8],
) -> Result<reqwest::Client, reqwest::Error> {
    let certificate = reqwest::Certificate::from_der(root_certificate_der)?;
    build_client_with_loopback_policy(
        reqwest::Client::builder().add_root_certificate(certificate),
        resolver,
        allow_loopback,
    )
}

/// Builds the pre-commit client with an additional PEM or DER trust anchor.
pub fn build_precommit_client_with_additional_ca(
    resolver: Arc<dyn DnsResolver>,
    allow_loopback: bool,
    certificate: &[u8],
) -> Result<reqwest::Client, DeliveryError> {
    let looks_like_pem = certificate
        .windows(b"-----BEGIN CERTIFICATE-----".len())
        .any(|window| window == b"-----BEGIN CERTIFICATE-----");
    let looks_like_der = certificate.first() == Some(&0x30);
    if certificate.is_empty() || (!looks_like_pem && !looks_like_der) {
        return Err(DeliveryError::Transport);
    }
    let certificate = reqwest::Certificate::from_pem(certificate)
        .or_else(|_| reqwest::Certificate::from_der(certificate))
        .map_err(|_| DeliveryError::Transport)?;
    build_client_with_loopback_policy(
        reqwest::Client::builder().add_root_certificate(certificate),
        resolver,
        allow_loopback,
    )
    .map_err(|_| DeliveryError::Transport)
}

fn build_client_with_loopback_policy(
    builder: reqwest::ClientBuilder,
    resolver: Arc<dyn DnsResolver>,
    allow_loopback: bool,
) -> Result<reqwest::Client, reqwest::Error> {
    builder
        .redirect(Policy::none())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .dns_resolver(Arc::new(ValidatingDnsResolver {
            resolver,
            allow_loopback,
        }))
        .build()
}

#[async_trait::async_trait]
impl HttpClient for ReqwestHttpClient {
    async fn post(
        &self,
        request: WebhookRequest,
        timeout: Duration,
    ) -> Result<WebhookResponse, DeliveryError> {
        if !endpoint_allowed(&request.endpoint) {
            return Err(DeliveryError::InvalidEndpoint);
        }
        let ClientState::Ready(client) = &self.client else {
            return Err(DeliveryError::Transport);
        };
        let response = client
            .post(&request.endpoint)
            .headers(request.headers.iter().try_fold(
                reqwest::header::HeaderMap::new(),
                |mut headers, (name, value)| {
                    let key = reqwest::header::HeaderName::from_bytes(name.as_bytes())
                        .map_err(|_| DeliveryError::InvalidHeader)?;
                    let value = reqwest::header::HeaderValue::from_str(value)
                        .map_err(|_| DeliveryError::InvalidHeader)?;
                    headers.insert(key, value);
                    Ok::<_, DeliveryError>(headers)
                },
            )?)
            .body(request.body)
            .timeout(timeout)
            .send()
            .await
            .map_err(|error| {
                if error.is_timeout() {
                    DeliveryError::Timeout
                } else {
                    DeliveryError::Transport
                }
            })?;
        let status = response.status().as_u16();
        if response
            .content_length()
            .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
        {
            return Err(DeliveryError::ResponseBodyTooLarge);
        }
        let mut body = response.bytes_stream();
        let mut body_size = 0usize;
        while let Some(chunk) = body.next().await {
            let chunk = chunk.map_err(|_| DeliveryError::Transport)?;
            body_size = body_size.saturating_add(chunk.len());
            if body_size > MAX_RESPONSE_BODY_BYTES {
                return Err(DeliveryError::ResponseBodyTooLarge);
            }
        }
        Ok(WebhookResponse { status })
    }
}

/// Delivery failure classification. Details intentionally contain no URL or
/// secret bytes so they are safe to persist as an error code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DeliveryError {
    /// The destination URL failed the egress policy.
    #[error("invalid webhook endpoint")]
    InvalidEndpoint,
    /// A request header could not be encoded.
    #[error("invalid webhook header")]
    InvalidHeader,
    /// The request exceeded its configured timeout.
    #[error("webhook delivery timed out")]
    Timeout,
    /// The endpoint transport failed.
    #[error("webhook transport failed")]
    Transport,
    /// Secret reference was not available.
    #[error("webhook signing secret unavailable")]
    SecretUnavailable,
    /// The endpoint returned more response data than the bounded client reads.
    #[error("webhook response body exceeded the configured limit")]
    ResponseBodyTooLarge,
    /// The destination circuit is open.
    #[error("webhook destination circuit is open")]
    CircuitOpen,
}

impl DeliveryError {
    fn code(self) -> &'static str {
        match self {
            Self::InvalidEndpoint => "invalid_endpoint",
            Self::InvalidHeader => "invalid_header",
            Self::Timeout => "timeout",
            Self::Transport => "transport_failure",
            Self::SecretUnavailable => "secret_unavailable",
            Self::ResponseBodyTooLarge => "response_body_too_large",
            Self::CircuitOpen => "circuit_open",
        }
    }
}

/// Result of one delivery invocation for an extension/event pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DeliveryOutcome {
    /// The endpoint accepted the event.
    Delivered,
    /// The current attempt failed terminally, or the caller exhausted its
    /// persisted attempt budget.
    DeadLettered,
    /// Delivery was rejected without an attempt because the circuit is open.
    CircuitOpen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AttemptOutcome {
    Delivered,
    RetryableFailure { error_code: &'static str },
    TerminalFailure { error_code: &'static str },
    CircuitOpen,
}

/// Polling worker that drains the durable extension outbox without putting
/// delivery on a state-transition critical path.
///
/// A worker owns one process-wide semaphore and one fair semaphore per
/// extension registration. The latter is intentionally keyed by the stable
/// registration ID rather than a URL: a registration is the durable ordering
/// domain even when its endpoint is edited.
pub struct DeliveryWorker<C, S, M, D> {
    engine: Arc<DeliveryEngine<C, S, M>>,
    store: Arc<D>,
    policy: DeliveryPolicy,
    concurrency: Arc<Semaphore>,
    worker_id: uuid::Uuid,
    destinations: Arc<StdMutex<HashMap<uuid::Uuid, std::sync::Weak<DestinationQueue>>>>,
    in_flight: Arc<StdMutex<usize>>,
    shutdown_drain_timeout: Duration,
}

impl<C, S, M, D> Clone for DeliveryWorker<C, S, M, D> {
    fn clone(&self) -> Self {
        Self {
            engine: Arc::clone(&self.engine),
            store: Arc::clone(&self.store),
            policy: self.policy,
            concurrency: Arc::clone(&self.concurrency),
            worker_id: self.worker_id,
            destinations: Arc::clone(&self.destinations),
            in_flight: Arc::clone(&self.in_flight),
            shutdown_drain_timeout: self.shutdown_drain_timeout,
        }
    }
}

impl<C, S, M, D> DeliveryWorker<C, S, M, D>
where
    C: HttpClient,
    S: SecretProvider,
    M: MetricsRecorder,
    D: ExtensionDeliveryStore,
{
    /// Creates a worker around a delivery engine and durable store.
    #[must_use]
    pub fn new(
        engine: Arc<DeliveryEngine<C, S, M>>,
        store: Arc<D>,
        policy: DeliveryPolicy,
    ) -> Self {
        Self {
            engine,
            store,
            policy,
            concurrency: Arc::new(Semaphore::new(policy.max_concurrency() as usize)),
            worker_id: uuid::Uuid::now_v7(),
            destinations: Arc::new(StdMutex::new(HashMap::new())),
            in_flight: Arc::new(StdMutex::new(0)),
            shutdown_drain_timeout: Duration::from_secs(30),
        }
    }

    /// Sets the maximum time spent draining in-flight requests after shutdown.
    #[must_use]
    pub fn with_shutdown_drain_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_drain_timeout = timeout;
        self
    }

    /// Runs until the watch channel receives `true`. Shutdown stops claiming
    /// new rows, drains the current batch up to its configured deadline, then
    /// cancels the remaining futures. Delivery rows are only acknowledged or
    /// rescheduled after their operation completes, so cancelled rows remain
    /// eligible for a later claim.
    pub async fn run(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        let mut ticker = tokio::time::interval(Duration::from_millis(100));
        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { return; }
                }
                _ = ticker.tick() => {
                    self.run_batch(shutdown.clone()).await;
                }
            }
        }
    }

    async fn run_batch(&self, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        if *shutdown.borrow() {
            return;
        }
        let now = self.engine.clock.now().as_offset_date_time();
        let lease_expires_at = now
            .checked_add(time::Duration::milliseconds(
                i64::try_from(self.policy.lease_ms()).unwrap_or(i64::MAX),
            ))
            .unwrap_or(now);
        let deliveries = match self
            .store
            .claim_due(now, self.worker_id, lease_expires_at, 64)
            .await
        {
            Ok(deliveries) => deliveries,
            Err(_) => return,
        };
        // The claimed rows are partitioned into explicit per-destination
        // queues. One queue task is spawned per destination, so rows for one
        // registration are processed in claim order while unrelated
        // registrations can use the process-wide semaphore concurrently.
        let mut queues: HashMap<uuid::Uuid, Vec<PendingExtensionDelivery>> = HashMap::new();
        for delivery in deliveries {
            queues
                .entry(delivery.registration.extension_id)
                .or_default()
                .push(delivery);
        }
        let mut tasks = tokio::task::JoinSet::new();
        for (extension_id, deliveries) in queues {
            let worker = self.clone();
            let queue = worker.destination_queue(extension_id);
            tasks.spawn(async move { worker.run_destination(queue, deliveries).await });
        }
        let mut stopping = *shutdown.borrow();
        let mut deadline =
            stopping.then(|| tokio::time::Instant::now() + self.shutdown_drain_timeout);
        while !tasks.is_empty() {
            if stopping {
                let Some(deadline) = deadline else {
                    break;
                };
                let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
                if remaining.is_zero() {
                    tracing::warn!(
                        event = "extension.delivery_shutdown_cancelled",
                        in_flight = tasks.len(),
                        detail = "delivery rows remain durable and will be reclaimed"
                    );
                    tasks.abort_all();
                    while let Some(result) = tasks.join_next().await {
                        self.record_task_result(result);
                    }
                    break;
                }
                tokio::select! {
                    result = tasks.join_next() => {
                        if let Some(result) = result {
                            self.record_task_result(result);
                        }
                    }
                    _ = tokio::time::sleep(remaining) => {}
                }
            } else {
                tokio::select! {
                    result = tasks.join_next() => {
                        if let Some(result) = result {
                            self.record_task_result(result);
                        }
                    }
                    changed = shutdown.changed() => {
                        stopping = changed.is_err() || *shutdown.borrow();
                        if stopping {
                            deadline = Some(tokio::time::Instant::now() + self.shutdown_drain_timeout);
                        }
                    }
                }
            }
        }
        self.cleanup_destination_queues();
        if self
            .store
            .purge_dead_letters(
                now - time::Duration::days(i64::from(self.policy.dlq_retention_days())),
            )
            .await
            .is_err()
        {
            tracing::warn!(event = "extension.dlq_purge_failed");
        }
    }

    fn record_task_result(&self, result: Result<Result<(), &'static str>, tokio::task::JoinError>) {
        match result {
            Ok(Ok(())) => {}
            Ok(Err(stage)) => {
                self.engine
                    .metrics
                    .incr(Counter::ExtensionDeliveryWorkerFailure);
                tracing::error!(
                    event = "extension.delivery_task_failed",
                    stage,
                    detail = "delivery remains leased until expiry"
                );
            }
            Err(error) => {
                self.engine
                    .metrics
                    .incr(Counter::ExtensionDeliveryWorkerFailure);
                tracing::error!(
                    event = "extension.delivery_task_join_failed",
                    panic = error.is_panic(),
                    cancelled = error.is_cancelled(),
                    error = %error
                );
            }
        }
    }

    fn destination_queue(&self, extension_id: uuid::Uuid) -> Arc<DestinationQueue> {
        let mut destinations = self
            .destinations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        destinations.retain(|_, queue| queue.strong_count() != 0);
        let queue = destinations.entry(extension_id).or_default().upgrade();
        if let Some(queue) = queue {
            return queue;
        }
        let queue = Arc::new(DestinationQueue {
            extension_id,
            gate: Mutex::new(()),
        });
        destinations.insert(extension_id, Arc::downgrade(&queue));
        queue
    }

    fn cleanup_destination_queues(&self) {
        let mut destinations = self
            .destinations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        destinations.retain(|_, queue| queue.strong_count() != 0);
    }

    async fn run_destination(
        &self,
        queue: Arc<DestinationQueue>,
        deliveries: Vec<PendingExtensionDelivery>,
    ) -> Result<(), &'static str> {
        let _gate = queue.gate.lock().await;
        tracing::debug!(
            event = "extension.delivery_destination_queue_started",
            extension_id = %queue.extension_id,
            queued = deliveries.len()
        );
        for delivery in deliveries {
            self.deliver_one(delivery).await?;
        }
        Ok(())
    }

    async fn deliver_one(&self, delivery: PendingExtensionDelivery) -> Result<(), &'static str> {
        let global_permit = match self.concurrency.clone().acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => {
                return Err("global semaphore closed");
            }
        };
        let _in_flight = InFlightGuard::new(
            Arc::clone(&self.in_flight),
            Arc::clone(&self.engine.metrics),
        );
        let _global_permit = global_permit;

        let attempt = delivery.attempt_count.saturating_add(1);
        tracing::debug!(
            event = "extension.delivery_attempt_scheduled",
            attempt,
            destination_isolation = true
        );
        let outcome = self
            .engine
            .deliver_attempt(
                &delivery.registration,
                delivery.event_id,
                &delivery.event_kind,
                &delivery.payload,
            )
            .await;
        let now = self.engine.clock.now().as_offset_date_time();
        match outcome {
            AttemptOutcome::Delivered => {
                self.store
                    .mark_delivered(
                        delivery.delivery_id,
                        delivery.lease_owner,
                        delivery.lease_token,
                        delivery.lease_expires_at,
                    )
                    .await
                    .map_err(|_| "mark delivered")?;
            }
            AttemptOutcome::CircuitOpen => {
                let next = now + time::Duration::milliseconds(self.policy.circuit_open_ms() as i64);
                self.store
                    .reschedule(
                        delivery.delivery_id,
                        delivery.lease_owner,
                        delivery.lease_token,
                        delivery.lease_expires_at,
                        delivery.attempt_count,
                        next,
                        "circuit_open",
                    )
                    .await
                    .map_err(|_| "circuit reschedule")?;
            }
            AttemptOutcome::TerminalFailure { error_code } => {
                self.store
                    .move_to_dead_letter(
                        delivery.delivery_id,
                        delivery.lease_owner,
                        delivery.lease_token,
                        delivery.lease_expires_at,
                        attempt,
                        error_code,
                        now + time::Duration::days(i64::from(self.policy.dlq_retention_days())),
                    )
                    .await
                    .map_err(|_| "terminal dead letter")?;
            }
            AttemptOutcome::RetryableFailure { error_code } => {
                if attempt >= self.policy.max_attempts() {
                    self.store
                        .move_to_dead_letter(
                            delivery.delivery_id,
                            delivery.lease_owner,
                            delivery.lease_token,
                            delivery.lease_expires_at,
                            attempt,
                            error_code,
                            now + time::Duration::days(i64::from(self.policy.dlq_retention_days())),
                        )
                        .await
                        .map_err(|_| "retry dead letter")?;
                } else {
                    let delay = full_jitter_delay(
                        self.policy.backoff_min_ms(),
                        self.policy.backoff_max_ms(),
                        delivery.attempt_count,
                    );
                    let next = now
                        .checked_add(time::Duration::try_from(delay).unwrap_or(time::Duration::MAX))
                        .unwrap_or(now);
                    tracing::debug!(
                        event = "extension.delivery_retry_scheduled",
                        attempt,
                        delay_ms = delay.as_millis() as u64
                    );
                    self.store
                        .reschedule(
                            delivery.delivery_id,
                            delivery.lease_owner,
                            delivery.lease_token,
                            delivery.lease_expires_at,
                            attempt,
                            next,
                            error_code,
                        )
                        .await
                        .map_err(|_| "retry reschedule")?;
                }
            }
        }
        Ok(())
    }
}

struct DestinationQueue {
    extension_id: uuid::Uuid,
    gate: Mutex<()>,
}

struct InFlightGuard<M: MetricsRecorder> {
    count: Arc<StdMutex<usize>>,
    metrics: Arc<M>,
}

impl<M> InFlightGuard<M>
where
    M: MetricsRecorder,
{
    fn new(count: Arc<StdMutex<usize>>, metrics: Arc<M>) -> Self {
        let mut count_guard = count.lock().unwrap_or_else(|error| error.into_inner());
        *count_guard += 1;
        let current = *count_guard;
        metrics.set(Gauge::ExtensionDeliveryInFlight, current as i64);
        drop(count_guard);
        Self { count, metrics }
    }
}

impl<M> Drop for InFlightGuard<M>
where
    M: MetricsRecorder,
{
    fn drop(&mut self) {
        let mut count = self.count.lock().unwrap_or_else(|error| error.into_inner());
        *count = count.saturating_sub(1);
        self.metrics
            .set(Gauge::ExtensionDeliveryInFlight, *count as i64);
    }
}

#[derive(Debug, Clone, Copy, Default)]
struct CircuitState {
    consecutive_failures: u32,
    opened_at: Option<OffsetDateTime>,
    half_open_probe: bool,
}

/// Signed webhook delivery engine.
pub struct DeliveryEngine<C, S, M> {
    client: Arc<C>,
    secrets: Arc<S>,
    metrics: Arc<M>,
    clock: Arc<dyn Clock>,
    policy: DeliveryPolicy,
    circuits: Mutex<HashMap<uuid::Uuid, CircuitState>>,
}

impl<C, S, M> DeliveryEngine<C, S, M>
where
    C: HttpClient,
    S: SecretProvider,
    M: MetricsRecorder,
{
    /// Creates a delivery engine with a system clock.
    #[must_use]
    pub fn new(client: Arc<C>, secrets: Arc<S>, metrics: Arc<M>, policy: DeliveryPolicy) -> Self {
        Self::with_clock(
            client,
            secrets,
            metrics,
            policy,
            Arc::new(SystemClock::new()),
        )
    }

    /// Creates a delivery engine with an injected clock.
    #[must_use]
    pub fn with_clock(
        client: Arc<C>,
        secrets: Arc<S>,
        metrics: Arc<M>,
        policy: DeliveryPolicy,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            client,
            secrets,
            metrics,
            clock,
            policy,
            circuits: Mutex::new(HashMap::new()),
        }
    }

    /// Delivers one event attempt with at-least-once semantics.
    ///
    /// Retry scheduling belongs to [`DeliveryWorker`]. Keeping one attempt in
    /// one future means a backoff never occupies a worker task or destination
    /// ordering permit.
    #[tracing::instrument(
        name = "extension_gateway.extension_delivery",
        skip_all,
        fields(message_size = tracing::field::Empty)
    )]
    pub async fn deliver(
        &self,
        registration: &ExtensionRegistration,
        event_id: uuid::Uuid,
        event_kind: &str,
        payload: &Value,
    ) -> DeliveryOutcome {
        match self
            .deliver_attempt(registration, event_id, event_kind, payload)
            .await
        {
            AttemptOutcome::Delivered => DeliveryOutcome::Delivered,
            AttemptOutcome::CircuitOpen => DeliveryOutcome::CircuitOpen,
            AttemptOutcome::RetryableFailure { .. } | AttemptOutcome::TerminalFailure { .. } => {
                DeliveryOutcome::DeadLettered
            }
        }
    }

    async fn deliver_attempt(
        &self,
        registration: &ExtensionRegistration,
        event_id: uuid::Uuid,
        event_kind: &str,
        payload: &Value,
    ) -> AttemptOutcome {
        if !registration.subscribed_events.contains(event_kind)
            || registration.status != crate::ExtensionStatus::Active
        {
            return AttemptOutcome::Delivered;
        }
        if !endpoint_allowed(&registration.endpoint) {
            self.metrics.incr(Counter::ExtensionDelivery {
                result: ExtensionDeliveryResult::Failure,
            });
            return AttemptOutcome::TerminalFailure {
                error_code: "invalid_endpoint",
            };
        }
        if !self.circuit_allows(registration.extension_id).await {
            return AttemptOutcome::CircuitOpen;
        }
        let secret = match self.secrets.resolve(&registration.signing_secret_ref).await {
            Ok(secret) => secret,
            Err(error) => {
                self.record_failure(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Failure,
                });
                tracing::warn!(
                    event = "extension.delivery_secret_unavailable",
                    code = error.code()
                );
                return AttemptOutcome::TerminalFailure {
                    error_code: error.code(),
                };
            }
        };

        let timestamp = self.clock.now().as_offset_date_time().unix_timestamp();
        let request = match build_signed_webhook(
            &registration.endpoint,
            event_id,
            event_kind,
            timestamp,
            payload,
            &secret,
        ) {
            Ok(request) => request,
            Err(error) => {
                self.record_failure(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Failure,
                });
                tracing::warn!(
                    event = "extension.delivery_request_rejected",
                    code = error.code()
                );
                return AttemptOutcome::TerminalFailure {
                    error_code: error.code(),
                };
            }
        };
        tracing::Span::current().record("message_size", request.body.len());
        match self
            .client
            .post(
                request,
                Duration::from_millis(u64::from(self.policy.timeout_ms())),
            )
            .await
        {
            Ok(response) if (200..=299).contains(&response.status) => {
                self.record_success(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Success,
                });
                AttemptOutcome::Delivered
            }
            Ok(_) => {
                self.record_failure(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Failure,
                });
                AttemptOutcome::RetryableFailure {
                    error_code: "delivery_failed",
                }
            }
            Err(DeliveryError::Timeout) => {
                self.record_failure(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Timeout,
                });
                AttemptOutcome::RetryableFailure {
                    error_code: "timeout",
                }
            }
            Err(
                error @ (DeliveryError::InvalidEndpoint
                | DeliveryError::InvalidHeader
                | DeliveryError::ResponseBodyTooLarge),
            ) => {
                self.record_failure(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Failure,
                });
                AttemptOutcome::TerminalFailure {
                    error_code: error.code(),
                }
            }
            Err(error) => {
                self.record_failure(registration.extension_id).await;
                self.metrics.incr(Counter::ExtensionDelivery {
                    result: ExtensionDeliveryResult::Failure,
                });
                tracing::debug!(
                    event = "extension.delivery_attempt_failed",
                    code = error.code()
                );
                AttemptOutcome::RetryableFailure {
                    error_code: error.code(),
                }
            }
        }
    }

    async fn circuit_allows(&self, extension_id: uuid::Uuid) -> bool {
        let now = self.clock.now().as_offset_date_time();
        let mut circuits = self.circuits.lock().await;
        let state = circuits.entry(extension_id).or_default();
        let Some(opened_at) = state.opened_at else {
            return true;
        };
        if now - opened_at < time::Duration::milliseconds(self.policy.circuit_open_ms() as i64) {
            return false;
        }
        if state.half_open_probe {
            return false;
        }
        state.half_open_probe = true;
        true
    }

    async fn record_success(&self, extension_id: uuid::Uuid) {
        let mut circuits = self.circuits.lock().await;
        circuits.insert(extension_id, CircuitState::default());
    }

    async fn record_failure(&self, extension_id: uuid::Uuid) {
        let now = self.clock.now().as_offset_date_time();
        let mut circuits = self.circuits.lock().await;
        let state = circuits.entry(extension_id).or_default();
        state.consecutive_failures = state.consecutive_failures.saturating_add(1);
        if state.consecutive_failures >= self.policy.circuit_failure_threshold() {
            state.opened_at = Some(now);
            state.half_open_probe = false;
        }
    }
}

/// Builds a webhook body and its HMAC-SHA-256 headers.
pub fn build_signed_webhook(
    endpoint: &str,
    event_id: uuid::Uuid,
    event_kind: &str,
    timestamp: i64,
    payload: &Value,
    secret: &[u8],
) -> Result<WebhookRequest, DeliveryError> {
    if !endpoint_allowed_syntax(endpoint) {
        return Err(DeliveryError::InvalidEndpoint);
    }
    let body = serde_json::to_vec(&json!({
        "event_id": event_id.to_string(),
        "event_kind": event_kind,
        "timestamp": timestamp,
        "payload": payload,
    }))
    .map_err(|_| DeliveryError::Transport)?;
    // allow-hardcoded-secret: signing input format contains no key material.
    let signing_input = format!("{timestamp}.{event_id}.{}", String::from_utf8_lossy(&body));
    let mut mac =
        HmacSha256::new_from_slice(secret).map_err(|_| DeliveryError::SecretUnavailable)?;
    mac.update(signing_input.as_bytes());
    let digest = mac.finalize().into_bytes();
    let signature = digest
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    Ok(WebhookRequest {
        endpoint: endpoint.to_owned(),
        headers: vec![
            ("Content-Type".to_owned(), "application/json".to_owned()),
            ("X-OrbiSync-Event-Id".to_owned(), event_id.to_string()),
            ("X-OrbiSync-Timestamp".to_owned(), timestamp.to_string()),
            (
                "X-OrbiSync-Signature".to_owned(),
                format!("sha256={signature}"),
            ),
        ],
        body,
    })
}

/// Calculates `random(0, min(max, min * 2^attempt))` as required by ADR-007.
#[must_use]
pub fn full_jitter_delay(min_ms: u64, max_ms: u64, attempt: u32) -> Duration {
    let exponential = min_ms.saturating_mul(1u64.checked_shl(attempt).unwrap_or(u64::MAX));
    let upper = exponential.min(max_ms);
    if upper == 0 {
        return Duration::ZERO;
    }
    let value = rand::random_range(0..=upper);
    Duration::from_millis(value)
}

fn endpoint_allowed(endpoint: &str) -> bool {
    endpoint_allowed_syntax(endpoint)
}

fn endpoint_allowed_syntax(endpoint: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(endpoint) else {
        return false;
    };
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.host_str().is_none_or(str::is_empty)
    {
        return false;
    }
    let Some(host) = url.host_str() else {
        return false;
    };
    if let Ok(ip) = host.parse::<IpAddr>() {
        return public_address(SocketAddr::new(
            ip,
            url.port_or_known_default().unwrap_or(443),
        ));
    }
    // Keep malformed hostnames out of the resolver boundary. This also
    // rejects URL forms that reqwest cannot represent as a DNS Name.
    host.parse::<reqwest::dns::Name>().is_ok()
}

fn public_address(address: SocketAddr) -> bool {
    match address.ip() {
        IpAddr::V4(ip) => public_ipv4(ip),
        IpAddr::V6(ip) => {
            // The whole IPv4-mapped block is special-purpose, regardless of
            // whether its embedded IPv4 address is globally reachable.
            if ipv6_special_purpose(ip).is_some_and(|global| !global) {
                return false;
            }
            // The well-known NAT64 prefix is globally reachable in IANA's
            // registry, but its low 32 bits identify the IPv4 destination.
            // Do not let a globally reachable NAT64 translator become a path
            // to a non-global IPv4 address.
            if ipv6_prefix_matches(ip, Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0), 96) {
                return public_ipv4(Ipv4Addr::from(
                    (u128::from(ip) & u128::from(u32::MAX)) as u32,
                ));
            }
            if let Some(ipv4) = embedded_ipv4(ip) {
                return public_ipv4(ipv4);
            }
            let segments = ip.segments();
            ipv6_special_purpose(ip).unwrap_or(true)
                && !(ip.is_loopback()
                    || ip.is_multicast()
                    || ip.is_unspecified()
                    || (segments[0] & 0xfe00) == 0xfc00 // unique local (fc00::/7)
                    || (segments[0] & 0xffc0) == 0xfe80 // link local (fe80::/10)
                    || (segments[0] & 0xffc0) == 0xfec0 // deprecated site local (fec0::/10)
                    || segments[..6].iter().all(|segment| *segment == 0)) // IPv4-compatible
        }
    }
}

fn public_ipv4(ip: Ipv4Addr) -> bool {
    if let Some(global) = ipv4_special_purpose(ip) {
        return global;
    }
    !(ip.is_private()
        || ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_unspecified())
}

/// Egress policy for IANA's special-purpose registries.
///
/// The policy is deliberately based on the registry's `Globally Reachable`
/// column, using longest-prefix matching for entries such as `2001::/23`
/// whose allocated sub-prefixes override the parent. Every listed prefix with
/// a false, blank, or `N/A` reachability value is rejected. Listed prefixes
/// with a true value remain eligible; they are special-purpose allocations,
/// but IANA explicitly marks them globally reachable. The globally reachable
/// NAT64 prefix is additionally checked against its embedded IPv4 destination.
/// Addresses not listed in the registry still have to pass the ordinary
/// unspecified, multicast, and local-address checks above. Keep these tables
/// synchronized with the current IANA IPv4/IPv6 Special-Purpose Address
/// Registries.
#[derive(Debug, Clone, Copy)]
struct Ipv4SpecialPurpose {
    network: [u8; 4],
    prefix_len: u8,
    globally_reachable: bool,
}

const IPV4_SPECIAL_PURPOSE: &[Ipv4SpecialPurpose] = &[
    Ipv4SpecialPurpose {
        network: [0, 0, 0, 0],
        prefix_len: 8,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [0, 0, 0, 0],
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [10, 0, 0, 0],
        prefix_len: 8,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [100, 64, 0, 0],
        prefix_len: 10,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [127, 0, 0, 0],
        prefix_len: 8,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [169, 254, 0, 0],
        prefix_len: 16,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 0],
        prefix_len: 24,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 0],
        prefix_len: 29,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 8],
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 9],
        prefix_len: 32,
        globally_reachable: true,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 10],
        prefix_len: 32,
        globally_reachable: true,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 170],
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 0, 171],
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 0, 2, 0],
        prefix_len: 24,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 31, 196, 0],
        prefix_len: 24,
        globally_reachable: true,
    },
    Ipv4SpecialPurpose {
        network: [192, 52, 193, 0],
        prefix_len: 24,
        globally_reachable: true,
    },
    Ipv4SpecialPurpose {
        network: [192, 88, 99, 0],
        prefix_len: 24,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 88, 99, 2],
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 168, 0, 0],
        prefix_len: 16,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [192, 175, 48, 0],
        prefix_len: 24,
        globally_reachable: true,
    },
    Ipv4SpecialPurpose {
        network: [198, 18, 0, 0],
        prefix_len: 15,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [198, 51, 100, 0],
        prefix_len: 24,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [203, 0, 113, 0],
        prefix_len: 24,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [240, 0, 0, 0],
        prefix_len: 4,
        globally_reachable: false,
    },
    Ipv4SpecialPurpose {
        network: [255, 255, 255, 255],
        prefix_len: 32,
        globally_reachable: false,
    },
];

fn ipv4_special_purpose(ip: Ipv4Addr) -> Option<bool> {
    IPV4_SPECIAL_PURPOSE
        .iter()
        .filter(|entry| ipv4_prefix_matches(ip, entry.network, entry.prefix_len))
        .max_by_key(|entry| entry.prefix_len)
        .map(|entry| entry.globally_reachable)
}

fn ipv4_prefix_matches(ip: Ipv4Addr, network: [u8; 4], prefix_len: u8) -> bool {
    let ip = u32::from(ip);
    let network = u32::from_be_bytes(network);
    let mask = if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    };
    ip & mask == network & mask
}

#[derive(Debug, Clone, Copy)]
struct Ipv6SpecialPurpose {
    network: Ipv6Addr,
    prefix_len: u8,
    globally_reachable: bool,
}

const IPV6_SPECIAL_PURPOSE: &[Ipv6SpecialPurpose] = &[
    Ipv6SpecialPurpose {
        network: Ipv6Addr::UNSPECIFIED,
        prefix_len: 128,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::LOCALHOST,
        prefix_len: 128,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0, 0),
        prefix_len: 96,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x64, 0xff9b, 0, 0, 0, 0, 0, 0),
        prefix_len: 96,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x64, 0xff9b, 1, 0, 0, 0, 0, 0),
        prefix_len: 48,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x100, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 64,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x100, 0, 0, 1, 0, 0, 0, 0),
        prefix_len: 64,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 23,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 1),
        prefix_len: 128,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 2),
        prefix_len: 128,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 1, 0, 0, 0, 0, 0, 3),
        prefix_len: 128,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 2, 0, 0, 0, 0, 0, 0),
        prefix_len: 48,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 3, 0, 0, 0, 0, 0, 0),
        prefix_len: 32,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 4, 0x112, 0, 0, 0, 0, 0),
        prefix_len: 48,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 0x10, 0, 0, 0, 0, 0, 0),
        prefix_len: 28,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 0x20, 0, 0, 0, 0, 0, 0),
        prefix_len: 28,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 0x30, 0, 0, 0, 0, 0, 0),
        prefix_len: 28,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2001, 0xdb8, 0, 0, 0, 0, 0, 0),
        prefix_len: 32,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2002, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 16,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x2620, 0x4f, 0x8000, 0, 0, 0, 0, 0),
        prefix_len: 48,
        globally_reachable: true,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x3fff, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 20,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0x5f00, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 16,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0xfc00, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 7,
        globally_reachable: false,
    },
    Ipv6SpecialPurpose {
        network: Ipv6Addr::new(0xfe80, 0, 0, 0, 0, 0, 0, 0),
        prefix_len: 10,
        globally_reachable: false,
    },
];

fn ipv6_special_purpose(ip: Ipv6Addr) -> Option<bool> {
    IPV6_SPECIAL_PURPOSE
        .iter()
        .filter(|entry| ipv6_prefix_matches(ip, entry.network, entry.prefix_len))
        .max_by_key(|entry| entry.prefix_len)
        .map(|entry| entry.globally_reachable)
}

fn ipv6_prefix_matches(ip: Ipv6Addr, network: Ipv6Addr, prefix_len: u8) -> bool {
    let ip = u128::from(ip);
    let network = u128::from(network);
    let mask = if prefix_len == 0 {
        0
    } else {
        u128::MAX << (128 - u32::from(prefix_len))
    };
    ip & mask == network & mask
}

/// Extracts IPv4 addresses represented by either IPv4-mapped or
/// IPv4-translated IPv6 forms. Treating these as IPv6 would otherwise bypass
/// the IPv4 special-use checks above.
fn embedded_ipv4(ip: Ipv6Addr) -> Option<Ipv4Addr> {
    let segments = ip.segments();
    let mapped = segments[..5].iter().all(|segment| *segment == 0) && segments[5] == 0xffff;
    let translated = segments[..4].iter().all(|segment| *segment == 0)
        && segments[4] == 0xffff
        && segments[5] == 0;
    (mapped || translated).then(|| {
        Ipv4Addr::new(
            (segments[6] >> 8) as u8,
            segments[6] as u8,
            (segments[7] >> 8) as u8,
            segments[7] as u8,
        )
    })
}

/// Reqwest resolver adapter that validates a complete DNS answer before it is
/// handed to hyper. The original URL remains untouched, preserving TLS SNI,
/// certificate verification, and the Host header while pinning the connector
/// to this verified address set.
struct ValidatingDnsResolver {
    resolver: Arc<dyn DnsResolver>,
    allow_loopback: bool,
}

impl reqwest::dns::Resolve for ValidatingDnsResolver {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let hostname = name.as_str().to_owned();
        let resolver = Arc::clone(&self.resolver);
        let allow_loopback = self.allow_loopback;
        Box::pin(async move {
            let addresses = resolver
                .resolve(&hostname)
                .await
                .map_err(|error| Box::new(error) as Box<dyn std::error::Error + Send + Sync>)?;
            if addresses.is_empty()
                || !addresses.iter().copied().all(|address| {
                    public_address(address) || (allow_loopback && address.ip().is_loopback())
                })
            {
                return Err(Box::new(DeliveryError::InvalidEndpoint)
                    as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(addresses.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeSet, HashMap};
    use std::io;
    use std::net::SocketAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use async_trait::async_trait;
    use hmac::Mac;
    use orbisync_application::metrics::NoopMetrics;
    use orbisync_application::{
        ApplicationError, ExtensionDeliveryStore, PendingExtensionDelivery,
    };
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;
    use tokio::sync::Mutex;
    use tokio_rustls::TlsAcceptor;

    use super::{
        DeliveryEngine, DeliveryError, DeliveryOutcome, DeliveryPolicy, DnsResolver, HttpClient,
        SecretProvider, SystemDnsResolver, ValidatingDnsResolver, WebhookResponse, build_client,
        build_test_client, full_jitter_delay, public_address,
    };
    use crate::{ExtensionRegistration, ExtensionStatus};

    #[derive(Debug, Default)]
    struct StatusClient {
        calls: AtomicUsize,
        statuses: Mutex<HashMap<String, u16>>,
    }

    #[async_trait]
    impl HttpClient for StatusClient {
        async fn post(
            &self,
            request: super::WebhookRequest,
            _timeout: std::time::Duration,
        ) -> Result<WebhookResponse, DeliveryError> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let statuses = self.statuses.lock().await;
            Ok(WebhookResponse {
                status: statuses.get(&request.endpoint).copied().unwrap_or(500),
            })
        }
    }

    #[derive(Debug, Clone, Copy)]
    struct TestSecrets;

    #[async_trait]
    impl SecretProvider for TestSecrets {
        async fn resolve(&self, _reference: &str) -> Result<Vec<u8>, DeliveryError> {
            Ok(b"unit-test-secret".to_vec())
        }
    }

    fn registration(extension_id: uuid::Uuid, endpoint: &str) -> ExtensionRegistration {
        ExtensionRegistration {
            extension_id,
            name: "test-extension".to_owned(),
            description: None,
            endpoint: endpoint.to_owned(),
            subscribed_events: BTreeSet::from(["user.created".to_owned()]),
            capabilities: BTreeSet::new(),
            token_scopes: BTreeSet::new(),
            status: ExtensionStatus::Active,
            signing_secret_ref: "ORBISYNC_TEST_WEBHOOK_SECRET".to_owned(),
        }
    }

    fn test_policy() -> DeliveryPolicy {
        DeliveryPolicy::new(100, 5)
            .expect("test policy")
            .with_backoff(0, 0)
    }

    fn test_engine<C>(
        client: Arc<C>,
        policy: DeliveryPolicy,
    ) -> DeliveryEngine<C, TestSecrets, NoopMetrics>
    where
        C: HttpClient,
    {
        DeliveryEngine::new(client, Arc::new(TestSecrets), Arc::new(NoopMetrics), policy)
    }

    #[test]
    fn jitter_is_bounded_and_not_fixed_to_the_base() {
        for attempt in 0..8 {
            let value = full_jitter_delay(1_000, 30_000, attempt).as_millis() as u64;
            let upper = (1_000u64.saturating_mul(1u64 << attempt)).min(30_000);
            assert!(value <= upper);
        }
        let samples: Vec<u64> = (0..256)
            .map(|_| full_jitter_delay(1_000, 30_000, 5).as_millis() as u64)
            .collect();
        assert!(samples.iter().any(|value| *value > 1_000));
        assert!(samples.iter().any(|value| *value < 30_000));
    }

    #[test]
    fn malformed_endpoint_is_rejected_before_signing() {
        let error = super::build_signed_webhook(
            "http://127.0.0.1:8080",
            uuid::Uuid::now_v7(),
            "user.created",
            1,
            &serde_json::json!({}),
            b"test",
        )
        .expect_err("HTTP endpoint must be rejected");
        assert_eq!(error, DeliveryError::InvalidEndpoint);
    }

    #[test]
    fn private_ip_endpoint_is_rejected_before_signing() {
        let error = super::build_signed_webhook(
            "https://127.0.0.1:8443",
            uuid::Uuid::now_v7(),
            "user.created",
            1,
            &serde_json::json!({}),
            b"test",
        )
        .expect_err("private IP endpoints must be rejected");
        assert_eq!(error, DeliveryError::InvalidEndpoint);
    }

    #[test]
    fn hostname_endpoint_is_syntax_checked_without_a_dns_preflight() {
        assert!(super::endpoint_allowed("https://webhook.example.test/hook"));
        assert!(!super::endpoint_allowed(
            "https://user:webhook@example.test/hook"
        ));
        assert!(!super::endpoint_allowed("https://example .test/hook"));
    }

    #[test]
    fn special_and_mapped_addresses_are_not_public() {
        let cases = [
            ("0.0.0.0:443", false),
            ("10.0.0.1:443", false),
            ("100.64.0.1:443", false),
            ("127.0.0.1:443", false),
            ("169.254.169.254:443", false),
            ("192.0.2.1:443", false),
            ("192.0.0.9:443", true),
            ("192.0.0.10:443", true),
            ("192.31.196.1:443", true),
            ("192.52.193.1:443", true),
            ("192.175.48.1:443", true),
            ("198.18.0.1:443", false),
            ("198.51.100.1:443", false),
            ("192.88.99.1:443", false),
            ("203.0.113.1:443", false),
            ("255.255.255.255:443", false),
            ("[::]:443", false),
            ("[::1]:443", false),
            ("[::ffff:1.1.1.1]:443", false),
            ("[::ffff:127.0.0.1]:443", false),
            ("[::ffff:0:127.0.0.1]:443", false),
            ("[fc00::1]:443", false),
            ("[fe80::1]:443", false),
            ("[2001:db8::1]:443", false),
            ("[2001:7::1]:443", false),
            ("[5f00::1]:443", false),
            ("[64:ff9b:1::1]:443", false),
            ("[64:ff9b::c000:0201]:443", false),
            ("[100::1]:443", false),
            ("[100:0:0:1::1]:443", false),
            ("[2001::1]:443", false),
            ("[2001:1::4]:443", false),
            ("[2001:1::1]:443", true),
            ("[2001:1::2]:443", true),
            ("[2001:1::3]:443", true),
            ("[2001:2::1]:443", false),
            ("[2001:3::1]:443", true),
            ("[2001:4:112::1]:443", true),
            ("[2001:10::1]:443", false),
            ("[2002:c000:0201::1]:443", false),
            ("[3fff::1]:443", false),
            // These entries are listed by IANA but have Globally Reachable=true.
            ("[64:ff9b::0808:0808]:443", true),
            ("[2001:20::1]:443", true),
            ("[2001:30::1]:443", true),
            ("[2620:4f:8000::1]:443", true),
            ("1.1.1.1:443", true),
            ("[2001:4860:4860::8888]:443", true),
        ];
        for (address, expected) in cases {
            assert!(
                public_address(address.parse().expect("socket address")) == expected,
                "{address} expected public_address={expected}"
            );
        }
    }

    #[derive(Debug)]
    struct InjectedResolver {
        answers: Mutex<Vec<Vec<SocketAddr>>>,
        calls: AtomicUsize,
    }

    #[async_trait]
    impl DnsResolver for InjectedResolver {
        async fn resolve(&self, _hostname: &str) -> Result<Vec<SocketAddr>, io::Error> {
            self.calls.fetch_add(1, Ordering::Relaxed);
            let mut answers = self.answers.lock().await;
            if answers.is_empty() {
                Err(io::Error::other("unexpected DNS lookup"))
            } else {
                Ok(answers.remove(0))
            }
        }
    }

    #[derive(Debug, Clone)]
    struct HttpsObservation {
        path: String,
        host: Option<String>,
        sni: Option<String>,
        peer: SocketAddr,
    }

    struct HttpsTestServer {
        address: SocketAddr,
        observations: Arc<Mutex<Vec<HttpsObservation>>>,
        connections: Arc<AtomicUsize>,
        task: tokio::task::JoinHandle<()>,
    }

    impl HttpsTestServer {
        async fn start() -> Self {
            let _provider_install_result =
                rustls::crypto::ring::default_provider().install_default();
            let cert = rcgen::generate_simple_self_signed(vec!["webhook.test".to_owned()])
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
            let observations = Arc::new(Mutex::new(Vec::new()));
            let connections = Arc::new(AtomicUsize::new(0));
            let task_observations = Arc::clone(&observations);
            let task_connections = Arc::clone(&connections);
            let task = tokio::spawn(async move {
                loop {
                    let Ok((socket, peer)) = listener.accept().await else {
                        return;
                    };
                    task_connections.fetch_add(1, Ordering::Relaxed);
                    let acceptor = acceptor.clone();
                    let observations = Arc::clone(&task_observations);
                    tokio::spawn(async move {
                        let Ok(mut stream) = acceptor.accept(socket).await else {
                            return;
                        };
                        let sni = stream.get_ref().1.server_name().map(str::to_owned);
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
                        let text = String::from_utf8_lossy(&request);
                        let mut lines = text.lines();
                        let path = lines
                            .next()
                            .and_then(|line| line.split_whitespace().nth(1))
                            .unwrap_or_default()
                            .to_owned();
                        let host = lines
                            .find_map(|line| {
                                let (name, value) = line.split_once(':')?;
                                name.eq_ignore_ascii_case("host").then_some(value.trim())
                            })
                            .map(str::to_owned);
                        let (status, headers) = if path == "/redirect" {
                            (
                                "302 Found",
                                "Location: https://redirect-target.test/final\r\n",
                            )
                        } else {
                            ("200 OK", "")
                        };
                        let response = format!(
                            "HTTP/1.1 {status}\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
                        );
                        if stream.write_all(response.as_bytes()).await.is_err() {
                            return;
                        }
                        observations.lock().await.push(HttpsObservation {
                            path,
                            host,
                            sni,
                            peer,
                        });
                    });
                }
            });
            Self {
                address,
                observations,
                connections,
                task,
            }
        }

        async fn observation(&self) -> HttpsObservation {
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                loop {
                    if let Some(observation) = self.observations.lock().await.first().cloned() {
                        return observation;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(1)).await;
                }
            })
            .await
            .expect("HTTPS request observation")
        }
    }

    impl Drop for HttpsTestServer {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    #[tokio::test]
    async fn validating_resolver_rejects_mixed_answers_and_rechecks_retries() {
        let resolver = Arc::new(InjectedResolver {
            answers: Mutex::new(vec![
                vec!["1.1.1.1:443".parse().expect("public address")],
                vec![
                    "1.1.1.1:443".parse().expect("public address"),
                    "10.0.0.1:443".parse().expect("private address"),
                ],
            ]),
            calls: AtomicUsize::new(0),
        });
        let validating = ValidatingDnsResolver {
            resolver: Arc::clone(&resolver) as Arc<dyn DnsResolver>,
            allow_loopback: false,
        };

        let name: reqwest::dns::Name = "webhook.test".parse().expect("DNS name");
        let first = reqwest::dns::Resolve::resolve(&validating, name)
            .await
            .expect("first answer is public")
            .collect::<Vec<_>>();
        assert_eq!(first, vec!["1.1.1.1:443".parse().expect("public address")]);

        let name: reqwest::dns::Name = "webhook.test".parse().expect("DNS name");
        assert!(
            reqwest::dns::Resolve::resolve(&validating, name)
                .await
                .is_err()
        );
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 2);
    }

    #[tokio::test]
    async fn injected_resolver_is_used_by_the_redirect_disabled_client() {
        let resolver = Arc::new(InjectedResolver {
            answers: Mutex::new(vec![vec!["10.0.0.1:443".parse().expect("private address")]]),
            calls: AtomicUsize::new(0),
        });
        let client =
            super::ReqwestHttpClient::with_resolver(Arc::clone(&resolver) as Arc<dyn DnsResolver>)
                .expect("client build");
        let request = super::WebhookRequest {
            endpoint: "https://webhook.test/hook".to_owned(),
            headers: Vec::new(),
            body: Vec::new(),
        };

        assert_eq!(
            client
                .post(request, std::time::Duration::from_millis(100))
                .await
                .expect_err("private DNS answer must fail closed"),
            DeliveryError::Transport
        );
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn mixed_dns_answer_is_rejected_before_any_connection() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");
        let private_address = listener.local_addr().expect("listener address");
        let resolver = Arc::new(InjectedResolver {
            answers: Mutex::new(vec![vec![
                "1.1.1.1:443".parse().expect("public address"),
                private_address,
            ]]),
            calls: AtomicUsize::new(0),
        });
        let client =
            super::ReqwestHttpClient::with_resolver(Arc::clone(&resolver) as Arc<dyn DnsResolver>)
                .expect("client build");

        let result = client
            .post(
                super::WebhookRequest {
                    endpoint: "https://webhook.test/hook".to_owned(),
                    headers: Vec::new(),
                    body: Vec::new(),
                },
                std::time::Duration::from_millis(100),
            )
            .await;
        assert_eq!(
            result.expect_err("mixed DNS answer must fail closed"),
            DeliveryError::Transport
        );
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 1);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err(),
            "a rejected private address must never be dialed"
        );
    }

    #[tokio::test]
    async fn real_engine_pins_verified_destination_and_preserves_sni_host_without_redirect() {
        let server = HttpsTestServer::start().await;
        let redirect_target = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("redirect target listener");
        let resolver = Arc::new(InjectedResolver {
            answers: Mutex::new(vec![
                vec![server.address],
                vec![
                    redirect_target
                        .local_addr()
                        .expect("redirect target address"),
                ],
            ]),
            calls: AtomicUsize::new(0),
        });
        // The loopback exception exists only in this test harness so the
        // actual reqwest resolver/connector path can use an in-process HTTPS
        // listener. Production build_client always uses allow_loopback=false.
        let raw_client = build_test_client(Arc::clone(&resolver) as Arc<dyn DnsResolver>)
            .expect("test client build");
        let client = Arc::new(super::ReqwestHttpClient {
            client: super::ClientState::Ready(raw_client),
        });
        let policy = DeliveryPolicy::new(500, 1)
            .expect("test policy")
            .with_backoff(0, 0);
        let engine =
            DeliveryEngine::new(client, Arc::new(TestSecrets), Arc::new(NoopMetrics), policy);
        let endpoint = format!("https://webhook.test:{}/redirect", server.address.port());

        assert_eq!(
            engine
                .deliver(
                    &registration(uuid::Uuid::now_v7(), &endpoint),
                    uuid::Uuid::now_v7(),
                    "user.created",
                    &json!({"id": "https"}),
                )
                .await,
            DeliveryOutcome::DeadLettered
        );
        let observation = server.observation().await;
        assert_eq!(observation.path, "/redirect");
        assert_eq!(observation.sni.as_deref(), Some("webhook.test"));
        assert_eq!(
            observation.host.as_deref(),
            Some(format!("webhook.test:{}", server.address.port()).as_str())
        );
        assert_eq!(
            observation.peer.ip(),
            "127.0.0.1".parse::<std::net::IpAddr>().expect("loopback")
        );
        assert_eq!(server.connections.load(Ordering::Relaxed), 1);
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 1);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                redirect_target.accept()
            )
            .await
            .is_err(),
            "redirect target must not be resolved or connected"
        );
    }

    #[tokio::test]
    async fn real_engine_re_resolves_and_rejects_private_rebinding() {
        let private_listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("private destination listener");
        let private_address = private_listener.local_addr().expect("private address");
        let resolver = Arc::new(InjectedResolver {
            answers: Mutex::new(vec![
                vec![SocketAddr::new(
                    "192.31.196.1".parse().expect("public registry address"),
                    private_address.port(),
                )],
                vec![private_address],
            ]),
            calls: AtomicUsize::new(0),
        });
        let client = Arc::new(
            super::ReqwestHttpClient::with_resolver(Arc::clone(&resolver) as Arc<dyn DnsResolver>)
                .expect("client build"),
        );
        let policy = DeliveryPolicy::new(100, 2)
            .expect("test policy")
            .with_backoff(0, 0);
        let engine =
            DeliveryEngine::new(client, Arc::new(TestSecrets), Arc::new(NoopMetrics), policy);
        let target = registration(uuid::Uuid::now_v7(), "https://webhook.test/hook");

        // `deliver()` performs exactly one attempt per call; retries are
        // issued by `DeliveryWorker` as independent later calls, each with
        // its own fresh resolve (see `deliver_one`). This models that: the
        // first resolved answer is a decoy that passes the public-address
        // check but is not reachable, so the attempt is dead-lettered without
        // ever dialing a real backend. The second `deliver()` call is the
        // retry a worker would issue, and its independent, fresh resolve is
        // where a DNS-rebinding attempt (a private address swapped in for
        // the second answer) must still be rejected before dialing.
        assert_eq!(
            engine
                .deliver(
                    &target,
                    uuid::Uuid::now_v7(),
                    "user.created",
                    &json!({"id": "decoy"})
                )
                .await,
            DeliveryOutcome::DeadLettered
        );
        assert_eq!(
            engine
                .deliver(
                    &target,
                    uuid::Uuid::now_v7(),
                    "user.created",
                    &json!({"id": "rebind"}),
                )
                .await,
            DeliveryOutcome::DeadLettered
        );
        assert_eq!(resolver.calls.load(Ordering::Relaxed), 2);
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(50),
                private_listener.accept()
            )
            .await
            .is_err(),
            "rejected rebinding destination must not be dialed"
        );
    }

    #[test]
    fn client_build_errors_are_returned_without_a_fallback() {
        let _resolver = SystemDnsResolver;
        let result = build_client(
            reqwest::Client::builder().use_preconfigured_tls(()),
            Arc::new(SystemDnsResolver),
        );
        assert!(result.is_err(), "unknown TLS backend must fail build");
    }

    #[test]
    fn infallible_constructors_retain_compatibility() {
        let _client = super::ReqwestHttpClient::new();
        let _default_client = super::ReqwestHttpClient::default();
    }

    #[tokio::test]
    async fn failed_client_sentinel_fails_closed_as_transport() {
        let client = super::ReqwestHttpClient::failed();
        let result = client
            .post(
                super::WebhookRequest {
                    endpoint: "https://webhook.test/hook".to_owned(),
                    headers: Vec::new(),
                    body: Vec::new(),
                },
                std::time::Duration::from_millis(100),
            )
            .await;
        assert_eq!(result, Err(DeliveryError::Transport));
    }

    #[test]
    fn timestamp_changes_the_signature() {
        let event_id = uuid::Uuid::now_v7();
        let first = super::build_signed_webhook(
            "https://1.1.1.1/webhook",
            event_id,
            "user.created",
            100,
            &json!({"id": "public"}),
            b"unit-test-secret",
        )
        .expect("first signature");
        let second = super::build_signed_webhook(
            "https://1.1.1.1/webhook",
            event_id,
            "user.created",
            101,
            &json!({"id": "public"}),
            b"unit-test-secret",
        )
        .expect("second signature");
        assert_ne!(first.headers[3].1, second.headers[3].1);

        let mut mac = super::HmacSha256::new_from_slice(b"unit-test-secret").expect("HMAC key");
        mac.update(
            format!(
                "100.{event_id}.{}",
                String::from_utf8(first.body.clone()).expect("JSON body")
            )
            .as_bytes(),
        );
        let expected = mac
            .finalize()
            .into_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(first.headers[3].1, format!("sha256={expected}"));
    }

    #[tokio::test]
    async fn circuit_breaker_is_per_destination() {
        let client = Arc::new(StatusClient::default());
        client.statuses.lock().await.extend([
            ("https://1.1.1.1/bad".to_owned(), 500),
            ("https://1.0.0.1/good".to_owned(), 204),
        ]);
        let engine = test_engine(Arc::clone(&client), test_policy());
        let bad = registration(uuid::Uuid::now_v7(), "https://1.1.1.1/bad");
        let good = registration(uuid::Uuid::now_v7(), "https://1.0.0.1/good");
        assert_ne!(bad.extension_id, good.extension_id);
        let event_id = uuid::Uuid::now_v7();

        for _ in 0..4 {
            assert_eq!(
                engine
                    .deliver(&bad, event_id, "user.created", &json!({}))
                    .await,
                DeliveryOutcome::DeadLettered
            );
        }
        assert_eq!(
            engine
                .deliver(&bad, event_id, "user.created", &json!({}))
                .await,
            DeliveryOutcome::DeadLettered
        );
        assert_eq!(
            engine
                .deliver(&bad, event_id, "user.created", &json!({}))
                .await,
            DeliveryOutcome::CircuitOpen
        );
        assert_eq!(
            engine
                .deliver(&good, event_id, "user.created", &json!({}))
                .await,
            DeliveryOutcome::Delivered
        );
        assert_eq!(client.calls.load(Ordering::Relaxed), 6);
    }

    #[derive(Debug, Default)]
    struct TestStore {
        claimed: AtomicUsize,
        dead_lettered: AtomicUsize,
    }

    #[async_trait]
    impl ExtensionDeliveryStore for TestStore {
        async fn claim_due(
            &self,
            now: time::OffsetDateTime,
            lease_owner: uuid::Uuid,
            lease_expires_at: time::OffsetDateTime,
            _limit: u32,
        ) -> Result<Vec<PendingExtensionDelivery>, ApplicationError> {
            let attempt_count = self.claimed.fetch_add(1, Ordering::Relaxed);
            if attempt_count < 5 {
                Ok(vec![PendingExtensionDelivery {
                    delivery_id: uuid::Uuid::now_v7(),
                    event_id: uuid::Uuid::now_v7(),
                    event_kind: "user.created".to_owned(),
                    payload: json!({"id": "public"}),
                    registration: registration(uuid::Uuid::now_v7(), "https://1.1.1.1/bad"),
                    attempt_count: attempt_count as u32,
                    available_at: now,
                    lease_owner,
                    lease_token: uuid::Uuid::now_v7(),
                    lease_expires_at,
                }])
            } else {
                Ok(Vec::new())
            }
        }

        async fn mark_delivered(
            &self,
            _delivery_id: uuid::Uuid,
            _lease_owner: uuid::Uuid,
            _lease_token: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
        ) -> Result<(), ApplicationError> {
            Ok(())
        }

        async fn reschedule(
            &self,
            _delivery_id: uuid::Uuid,
            _lease_owner: uuid::Uuid,
            _lease_token: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
            _attempt_count: u32,
            _available_at: time::OffsetDateTime,
            _error_code: &str,
        ) -> Result<(), ApplicationError> {
            Ok(())
        }

        async fn move_to_dead_letter(
            &self,
            _delivery_id: uuid::Uuid,
            _lease_owner: uuid::Uuid,
            _lease_token: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
            _attempt_count: u32,
            _error_code: &str,
            _retained_until: time::OffsetDateTime,
        ) -> Result<(), ApplicationError> {
            self.dead_lettered.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn purge_dead_letters(
            &self,
            _before: time::OffsetDateTime,
        ) -> Result<u64, ApplicationError> {
            Ok(0)
        }
    }

    #[tokio::test]
    async fn five_failures_are_recorded_in_the_dlq() {
        let client = Arc::new(StatusClient::default());
        client
            .statuses
            .lock()
            .await
            .insert("https://1.1.1.1/bad".to_owned(), 500);
        let policy = test_policy();
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(TestStore::default());
        let worker = super::DeliveryWorker::new(Arc::clone(&engine), Arc::clone(&store), policy);
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { worker.run(shutdown_rx).await });
        tokio::time::sleep(std::time::Duration::from_millis(550)).await;
        shutdown_tx.send(true).expect("shutdown worker");
        task.await.expect("worker joined");
        assert_eq!(client.calls.load(Ordering::Relaxed), 5);
        assert_eq!(store.dead_lettered.load(Ordering::Relaxed), 1);
    }

    #[derive(Debug, Default)]
    struct TimingClient {
        active: AtomicUsize,
        max_active: AtomicUsize,
        completed: Mutex<Vec<String>>,
        delays: Mutex<HashMap<String, std::time::Duration>>,
    }

    #[async_trait]
    impl HttpClient for TimingClient {
        async fn post(
            &self,
            request: super::WebhookRequest,
            _timeout: std::time::Duration,
        ) -> Result<WebhookResponse, DeliveryError> {
            let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
            self.max_active.fetch_max(active, Ordering::AcqRel);
            let delay = self
                .delays
                .lock()
                .await
                .get(&request.endpoint)
                .copied()
                .unwrap_or_default();
            tokio::time::sleep(delay).await;
            self.completed.lock().await.push(
                request
                    .headers
                    .iter()
                    .find(|(name, _)| name == "X-OrbiSync-Event-Id")
                    .map(|(_, value)| value.clone())
                    .unwrap_or_default(),
            );
            self.active.fetch_sub(1, Ordering::AcqRel);
            Ok(WebhookResponse { status: 204 })
        }
    }

    #[derive(Debug, Default)]
    struct BatchStore {
        deliveries: Mutex<Vec<PendingExtensionDelivery>>,
        delivered: AtomicUsize,
        rescheduled: Mutex<Vec<(u32, time::OffsetDateTime, String)>>,
    }

    #[async_trait]
    impl ExtensionDeliveryStore for BatchStore {
        async fn claim_due(
            &self,
            _now: time::OffsetDateTime,
            _lease_owner: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
            _limit: u32,
        ) -> Result<Vec<PendingExtensionDelivery>, ApplicationError> {
            Ok(std::mem::take(&mut *self.deliveries.lock().await))
        }

        async fn mark_delivered(
            &self,
            _delivery_id: uuid::Uuid,
            _lease_owner: uuid::Uuid,
            _lease_token: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
        ) -> Result<(), ApplicationError> {
            self.delivered.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }

        async fn reschedule(
            &self,
            _delivery_id: uuid::Uuid,
            _lease_owner: uuid::Uuid,
            _lease_token: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
            attempt_count: u32,
            available_at: time::OffsetDateTime,
            error_code: &str,
        ) -> Result<(), ApplicationError> {
            self.rescheduled.lock().await.push((
                attempt_count,
                available_at,
                error_code.to_owned(),
            ));
            Ok(())
        }

        async fn move_to_dead_letter(
            &self,
            _delivery_id: uuid::Uuid,
            _lease_owner: uuid::Uuid,
            _lease_token: uuid::Uuid,
            _lease_expires_at: time::OffsetDateTime,
            _attempt_count: u32,
            _error_code: &str,
            _retained_until: time::OffsetDateTime,
        ) -> Result<(), ApplicationError> {
            Ok(())
        }

        async fn purge_dead_letters(
            &self,
            _before: time::OffsetDateTime,
        ) -> Result<u64, ApplicationError> {
            Ok(0)
        }
    }

    fn pending(
        extension_id: uuid::Uuid,
        endpoint: &str,
        event_id: uuid::Uuid,
    ) -> PendingExtensionDelivery {
        PendingExtensionDelivery {
            delivery_id: uuid::Uuid::now_v7(),
            event_id,
            event_kind: "user.created".to_owned(),
            payload: json!({"id": "public"}),
            registration: registration(extension_id, endpoint),
            attempt_count: 0,
            available_at: time::OffsetDateTime::now_utc(),
            lease_owner: uuid::Uuid::now_v7(),
            lease_token: uuid::Uuid::now_v7(),
            lease_expires_at: time::OffsetDateTime::now_utc() + time::Duration::minutes(1),
        }
    }

    #[tokio::test]
    async fn slow_destination_does_not_serialize_healthy_destination() {
        let client = Arc::new(TimingClient::default());
        client.delays.lock().await.insert(
            "https://1.1.1.1/slow".to_owned(),
            std::time::Duration::from_millis(100),
        );
        let policy = DeliveryPolicy::new_with_concurrency(1_000, 1, 2)
            .expect("valid concurrency policy")
            .with_backoff(0, 0);
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        let slow_id = uuid::Uuid::now_v7();
        let healthy_id = uuid::Uuid::now_v7();
        store.deliveries.lock().await.extend([
            pending(uuid::Uuid::now_v7(), "https://1.1.1.1/slow", slow_id),
            pending(uuid::Uuid::now_v7(), "https://1.0.0.1/healthy", healthy_id),
        ]);
        let worker = super::DeliveryWorker::new(engine, Arc::clone(&store), policy);
        let shutdown = tokio::sync::watch::channel(false).1;
        worker.run_batch(shutdown).await;
        let completed = client.completed.lock().await.clone();
        assert_eq!(completed, vec![healthy_id.to_string(), slow_id.to_string()]);
        assert_eq!(client.max_active.load(Ordering::Acquire), 2);
    }

    #[tokio::test]
    async fn same_destination_is_serial_and_retry_is_persisted_without_sleeping() {
        let client = Arc::new(StatusClient::default());
        let endpoint = "https://1.1.1.1/ordered";
        client
            .statuses
            .lock()
            .await
            .insert(endpoint.to_owned(), 500);
        let policy = DeliveryPolicy::new_with_concurrency(1_000, 3, 2)
            .expect("valid concurrency policy")
            .with_backoff(5_000, 5_000);
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        let extension_id = uuid::Uuid::now_v7();
        store.deliveries.lock().await.extend([
            pending(extension_id, endpoint, uuid::Uuid::now_v7()),
            pending(extension_id, endpoint, uuid::Uuid::now_v7()),
        ]);
        let worker = super::DeliveryWorker::new(engine, Arc::clone(&store), policy);
        worker.run_batch(tokio::sync::watch::channel(false).1).await;
        assert_eq!(client.calls.load(Ordering::Acquire), 2);
        let rescheduled = store.rescheduled.lock().await;
        assert_eq!(rescheduled.len(), 2);
        assert!(
            rescheduled
                .iter()
                .all(|(attempt, _, code)| { *attempt == 1 && code == "delivery_failed" })
        );
    }

    #[tokio::test]
    async fn process_wide_concurrency_cap_is_never_exceeded() {
        let client = Arc::new(TimingClient::default());
        for suffix in ["a", "b", "c", "d"] {
            client.delays.lock().await.insert(
                format!("https://1.1.1.1/{suffix}"),
                std::time::Duration::from_millis(30),
            );
        }
        let policy = DeliveryPolicy::new_with_concurrency(1_000, 1, 2)
            .expect("valid concurrency policy")
            .with_backoff(0, 0);
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        for suffix in ["a", "b", "c", "d"] {
            store.deliveries.lock().await.push(pending(
                uuid::Uuid::now_v7(),
                &format!("https://1.1.1.1/{suffix}"),
                uuid::Uuid::now_v7(),
            ));
        }
        let worker = super::DeliveryWorker::new(engine, Arc::clone(&store), policy);
        worker.run_batch(tokio::sync::watch::channel(false).1).await;
        assert!(client.max_active.load(Ordering::Acquire) <= 2);
        assert_eq!(store.delivered.load(Ordering::Acquire), 4);
    }

    #[tokio::test]
    async fn same_destination_queue_preserves_claim_order() {
        let client = Arc::new(TimingClient::default());
        let policy = test_policy();
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        let extension_id = uuid::Uuid::now_v7();
        let first = uuid::Uuid::now_v7();
        let second = uuid::Uuid::now_v7();
        store.deliveries.lock().await.extend([
            pending(extension_id, "https://1.1.1.1/ordered", first),
            pending(extension_id, "https://1.1.1.1/ordered", second),
        ]);
        let worker = super::DeliveryWorker::new(engine, Arc::clone(&store), policy);
        worker.run_batch(tokio::sync::watch::channel(false).1).await;
        assert_eq!(
            client.completed.lock().await.as_slice(),
            [first.to_string(), second.to_string()]
        );
    }

    #[tokio::test]
    async fn idle_destination_queue_entries_are_evicted() {
        let policy = test_policy();
        let client = Arc::new(StatusClient::default());
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        let worker = super::DeliveryWorker::new(engine, store, policy);
        let queue = worker.destination_queue(uuid::Uuid::now_v7());
        assert_eq!(worker.destinations.lock().expect("map lock").len(), 1);
        drop(queue);
        worker.cleanup_destination_queues();
        assert!(worker.destinations.lock().expect("map lock").is_empty());
    }

    #[tokio::test]
    #[allow(clippy::panic)]
    async fn task_panic_and_cancel_are_recorded_and_permits_are_released() {
        let policy = test_policy();
        let client = Arc::new(StatusClient::default());
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        let worker = super::DeliveryWorker::new(engine, store, policy);
        let permit = worker
            .concurrency
            .clone()
            .acquire_owned()
            .await
            .expect("global permit");
        let panicked = tokio::spawn(async move {
            drop(permit);
            panic!("test task panic");
        })
        .await;
        worker.record_task_result(panicked.map(|_| Ok(())));
        assert!(worker.concurrency.try_acquire().is_ok());

        let cancelled = tokio::spawn(async {
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            Ok::<(), &'static str>(())
        });
        cancelled.abort();
        worker.record_task_result(cancelled.await.map(|_| Ok(())));
    }

    #[tokio::test]
    async fn shutdown_cancels_in_flight_and_leaves_rows_unacknowledged() {
        let client = Arc::new(TimingClient::default());
        client.delays.lock().await.insert(
            "https://1.1.1.1/slow".to_owned(),
            std::time::Duration::from_secs(5),
        );
        let policy =
            DeliveryPolicy::new_with_concurrency(10_000, 1, 1).expect("valid concurrency policy");
        let engine = Arc::new(test_engine(Arc::clone(&client), policy));
        let store = Arc::new(BatchStore::default());
        store.deliveries.lock().await.push(pending(
            uuid::Uuid::now_v7(),
            "https://1.1.1.1/slow",
            uuid::Uuid::now_v7(),
        ));
        let worker = super::DeliveryWorker::new(engine, Arc::clone(&store), policy)
            .with_shutdown_drain_timeout(std::time::Duration::from_millis(10));
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let task = tokio::spawn(async move { worker.run_batch(shutdown_rx).await });
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        shutdown_tx.send(true).expect("shutdown");
        task.await.expect("batch joined");
        assert_eq!(store.delivered.load(Ordering::Acquire), 0);
    }
}
