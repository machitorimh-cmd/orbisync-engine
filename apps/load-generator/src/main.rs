//! OrbiSync load generator — real traffic driver.
//!
//! Drives the public contract only (OpenAPI + `realtime.proto`):
//! `login -> realtime ticket -> WebSocket (ClientHello -> JoinInstance) -> 10 Hz TransformInput`
//! for `N` concurrent virtual users.
//!
//! This binary performs real network I/O and measures what is observed:
//! connection and join latencies, round-trip of transform updates, messages
//! received, and failure counts. The previous stub's XorShift PRNG path has
//! been removed entirely.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use futures_util::{Sink, SinkExt, StreamExt};
use prost::Message as ProstMessage;
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Semaphore};

// ---------------------------------------------------------------------------
// CLI
// ---------------------------------------------------------------------------

/// Real load generator — drives the public OrbiSync contract.
#[derive(Clone, Parser)]
#[command(name = "orbisync-load-generator", version, about)]
struct Args {
    /// Base HTTP URL of the server, e.g. `http://127.0.0.1:8080`.
    #[arg(long, default_value = "http://127.0.0.1:8080")]
    server_url: String,

    /// WebSocket URL override. When empty, derived from `server_url` as `ws(s)://host/ws`.
    #[arg(long, default_value = "")]
    ws_url: String,

    /// Number of concurrent virtual users.
    #[arg(long, default_value_t = 50)]
    users: usize,

    /// Transform send rate per user in Hz.
    #[arg(long, default_value_t = 10.0)]
    hz: f64,

    /// Test duration in seconds.
    #[arg(long, default_value_t = 10)]
    duration: u64,

    /// Maximum number of login requests in flight. This protects the server's
    /// bounded password-hash capacity without changing the server setting.
    #[arg(long, default_value_t = 4)]
    login_concurrency: usize,

    /// Avatar placement. `grid` spreads users beyond the visibility radius.
    #[arg(long, default_value = "grid")]
    placement: String,

    /// Grid spacing in metres for `grid` placement.
    #[arg(long, default_value_t = 7.5)]
    placement_spacing: f64,

    /// Add an x-axis offset in metres to the selected avatar placement.
    #[arg(long, default_value_t = 0.0)]
    placement_offset_x: f64,

    /// Add a z-axis offset in metres to the selected avatar placement.
    #[arg(long, default_value_t = 0.0)]
    placement_offset_z: f64,

    /// Login identifier for all virtual users.
    #[arg(long, default_value = "admin")]
    login_id: String,

    /// Password for `login_id`. Use env `ORBISYNC_LOAD_PASSWORD` or `--password-file` to avoid leaking via process list / shell history.
    #[arg(
        long,
        env = "ORBISYNC_LOAD_PASSWORD",
        hide_env_values = true,
        default_value = ""
    )]
    password: String,

    /// File containing the password (takes precedence over `--password` / `ORBISYNC_LOAD_PASSWORD`).
    #[arg(long)]
    password_file: Option<String>,

    /// Existing world instance to join. When empty, the generator creates a world+instance.
    #[arg(long, default_value = "")]
    instance_id: String,

    /// Git commit under test (recorded in the report). Defaults to `$GIT_COMMIT` or `unknown`.
    #[arg(long, default_value = "")]
    commit: String,

    /// Do not create world/instance even if `instance_id` is empty. Fails if instance is missing.
    #[arg(long, default_value_t = false)]
    no_auto_create: bool,

    /// Load scenario from the seven scenarios in test-and-ci.md.
    #[arg(long, default_value = "transform")]
    scenario: String,

    /// Connection ramp rate for the `ramp` scenario (connections per second).
    #[arg(long, default_value_t = 50.0)]
    ramp_rate: f64,

    /// Percentage of users that reconnect in the `reconnect` scenario.
    #[arg(long, default_value_t = 10.0)]
    reconnect_percent: f64,

    /// Seconds between reconnect attempts in the `reconnect` scenario.
    #[arg(long, default_value_t = 10)]
    reconnect_interval: u64,

    /// Percentage of users that intentionally delay reads in `slow-consumer`.
    #[arg(long, default_value_t = 5.0)]
    slow_consumer_percent: f64,

    /// Delay before a slow consumer reads the socket again.
    #[arg(long, default_value_t = 250)]
    slow_consumer_delay_ms: u64,

    /// Internal Prometheus endpoint. If omitted, server-side metrics are unavailable.
    #[arg(long)]
    metrics_url: Option<String>,

    /// Interval between metrics scrapes while the test is running.
    #[arg(long, default_value_t = 5)]
    metrics_interval: u64,

    /// Write the machine-readable report to this path.
    #[arg(long)]
    json_output: Option<String>,
}

impl std::fmt::Debug for Args {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Args")
            .field("server_url", &self.server_url)
            .field("ws_url", &self.ws_url)
            .field("users", &self.users)
            .field("hz", &self.hz)
            .field("duration", &self.duration)
            .field("login_concurrency", &self.login_concurrency)
            .field("placement", &self.placement)
            .field("placement_spacing", &self.placement_spacing)
            .field("placement_offset_x", &self.placement_offset_x)
            .field("placement_offset_z", &self.placement_offset_z)
            .field("login_id", &self.login_id)
            .field("password", &"[REDACTED]")
            .field("password_file", &self.password_file)
            .field("instance_id", &self.instance_id)
            .field("commit", &self.commit)
            .field("no_auto_create", &self.no_auto_create)
            .field("scenario", &self.scenario)
            .field("ramp_rate", &self.ramp_rate)
            .field("reconnect_percent", &self.reconnect_percent)
            .field("reconnect_interval", &self.reconnect_interval)
            .field("slow_consumer_percent", &self.slow_consumer_percent)
            .field("slow_consumer_delay_ms", &self.slow_consumer_delay_ms)
            .field("metrics_url", &self.metrics_url)
            .field("metrics_interval", &self.metrics_interval)
            .field("json_output", &self.json_output)
            .finish()
    }
}

// ---------------------------------------------------------------------------
// Report structures
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct UserResult {
    login_ok: bool,
    ticket_ok: bool,
    ws_connected: bool,
    hello_ok: bool,
    join_ok: bool,
    join_latency: Option<Duration>,
    transform_rtts: Vec<Duration>,
    messages_received: usize,
    errors: usize,
    dropped_updates: usize,
    sent_bytes: u64,
    received_bytes: u64,
    snapshot_sizes: Vec<usize>,
    disconnects: BTreeMap<String, usize>,
    reconnects: usize,
    ramp_stage: Option<usize>,
    ramp_start_delay_ms: Option<u128>,
}

const SERVER_METRICS: [&str; 15] = [
    "process_cpu_seconds_total",
    "process_resident_memory_bytes",
    "instance_command_queue_depth",
    "instance_mailbox_saturated_total",
    "instance_mailbox_dropped_total",
    "http_requests_total",
    "http_request_duration_seconds",
    "auth_login_failures_total",
    "rate_limit_rejected_total",
    "db_query_duration_seconds",
    "extension_delivery_total",
    "outbound_queue_depth",
    "outbound_queue_depth_max",
    "outbound_queue_bytes",
    "outbound_queue_bytes_max",
];

#[derive(Debug, Clone, Serialize)]
struct MetricsSample {
    captured_at_unix_ms: i64,
    elapsed_ms: u128,
    scrape_ok: bool,
    values: BTreeMap<String, f64>,
    unavailable_metrics: Vec<String>,
    error: Option<String>,
    cpu_rate_per_second: Option<f64>,
}

#[derive(Debug, Clone, Serialize)]
struct MetricsReport {
    configured: bool,
    url: Option<String>,
    interval_seconds: u64,
    samples: Vec<MetricsSample>,
}

struct MetricsCollector {
    report: MetricsReport,
    started: Instant,
    last_cpu: Option<(f64, i64)>,
}

impl MetricsCollector {
    fn new(url: Option<String>, interval_seconds: u64, started: Instant) -> Self {
        Self {
            report: MetricsReport {
                configured: url.is_some(),
                url,
                interval_seconds,
                samples: Vec::new(),
            },
            started,
            last_cpu: None,
        }
    }

    fn unavailable_sample(&mut self, error: impl Into<String>) {
        self.report.samples.push(MetricsSample {
            captured_at_unix_ms: current_unix_ms(),
            elapsed_ms: self.started.elapsed().as_millis(),
            scrape_ok: false,
            values: BTreeMap::new(),
            unavailable_metrics: SERVER_METRICS.iter().map(|s| (*s).to_string()).collect(),
            error: Some(error.into()),
            cpu_rate_per_second: None,
        });
    }

    fn record(&mut self, text: &str, captured_at_unix_ms: i64) {
        let values = parse_metrics(text);
        let cpu = values.get("process_cpu_seconds_total").copied();
        let cpu_rate = cpu.and_then(|now| {
            let previous = self.last_cpu.replace((now, captured_at_unix_ms));
            previous.and_then(|(old, old_at)| {
                let seconds = (captured_at_unix_ms - old_at) as f64 / 1000.0;
                (seconds > 0.0).then_some((now - old) / seconds)
            })
        });
        if cpu.is_none() {
            self.last_cpu = None;
        }
        let unavailable_metrics = SERVER_METRICS
            .iter()
            .filter(|name| !values.keys().any(|key| metric_name(key) == **name))
            .map(|name| (*name).to_string())
            .collect();
        self.report.samples.push(MetricsSample {
            captured_at_unix_ms,
            elapsed_ms: self.started.elapsed().as_millis(),
            scrape_ok: true,
            values,
            unavailable_metrics,
            error: None,
            cpu_rate_per_second: cpu_rate,
        });
    }
}

fn metric_name(token: &str) -> &str {
    token.split('{').next().unwrap_or(token)
}

fn parse_metrics(text: &str) -> BTreeMap<String, f64> {
    let mut values = BTreeMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split_whitespace();
        let Some(name_with_labels) = parts.next() else {
            continue;
        };
        let Some(raw_value) = parts.next() else {
            continue;
        };
        let name = metric_name(name_with_labels);
        if !SERVER_METRICS.contains(&name) {
            continue;
        }
        let Ok(value) = raw_value.parse::<f64>() else {
            continue;
        };
        // Histograms and labelled gauges can have multiple samples. Keep the
        // wire labels in the key and aggregate exact duplicates only.
        *values.entry(name_with_labels.to_string()).or_insert(0.0) += value;
    }
    values
}

async fn scrape_metrics(client: &reqwest::Client, collector: &Arc<Mutex<MetricsCollector>>) {
    let url = {
        let guard = collector.lock().await;
        guard.report.url.clone()
    };
    let Some(url) = url else {
        let mut guard = collector.lock().await;
        if guard.report.samples.is_empty() {
            guard.unavailable_sample("server-side metrics unavailable: --metrics-url was not set");
        }
        return;
    };
    match client.get(url).send().await {
        Ok(response) => {
            let status = response.status();
            match response.text().await {
                Ok(body) if status.is_success() => {
                    let captured_at = current_unix_ms();
                    collector.lock().await.record(&body, captured_at);
                }
                Ok(body) => collector.lock().await.unavailable_sample(format!(
                    "metrics scrape returned {} ({} bytes)",
                    status,
                    body.len()
                )),
                Err(error) => collector
                    .lock()
                    .await
                    .unavailable_sample(format!("metrics scrape body failed: {error}")),
            }
        }
        Err(error) => collector
            .lock()
            .await
            .unavailable_sample(format!("metrics scrape failed: {error}")),
    }
}

// ---------------------------------------------------------------------------
// Proto definitions
// ---------------------------------------------------------------------------

const PROTOCOL_MAJOR: u32 = 1;
const WEBSOCKET_SUBPROTOCOL: &str = "orbisync.v1.protobuf";

// `proto/orbisync/v1/realtime.proto` is the source of truth. Keep this
// standalone load-generator crate in lockstep with the server by generating
// its wire types at build time rather than maintaining a second schema.
#[allow(missing_docs, clippy::all, clippy::pedantic, unused_qualifications)]
mod realtime {
    include!(concat!(env!("OUT_DIR"), "/orbisync.v1.rs"));
}

use realtime::{ClientHello, Envelope, JoinInstance, Transform, TransformInput, envelope};

// ---------------------------------------------------------------------------
// HTTP DTOs
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct LoginRequest {
    login_id: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct LoginResponse {
    access_token: String,
    #[allow(dead_code)]
    token_type: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct CreatedUserResponse {
    user: CreatedUser,
    temporary_password: String,
}

#[derive(Debug, Deserialize)]
struct CreatedUser {
    id: String,
    login_id: String,
}

#[derive(Clone)]
struct VirtualUserCredential {
    user_id: String,
    login_id: String,
    password: String,
}

#[derive(Debug, Deserialize)]
struct TicketResponse {
    realtime_ticket: Option<String>,
    ticket: Option<String>,
    #[allow(dead_code)]
    expires_in: Option<u64>,
    #[allow(dead_code)]
    expires_at: Option<String>,
}

impl TicketResponse {
    fn ticket(&self) -> Option<&str> {
        self.realtime_ticket.as_deref().or(self.ticket.as_deref())
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn http_origin(server_url: &str) -> String {
    server_url.trim_end_matches('/').to_string()
}

fn ws_url_from(server_url: &str, override_ws: &str) -> String {
    if !override_ws.is_empty() {
        return override_ws.to_string();
    }
    let http = http_origin(server_url);
    if http.starts_with("https://") {
        format!("wss://{}/ws", &http["https://".len()..])
    } else if http.starts_with("http://") {
        format!("ws://{}/ws", &http["http://".len()..])
    } else {
        format!("ws://{http}/ws")
    }
}

fn initial_position(
    placement: &str,
    idx: usize,
    users: usize,
    spacing: f64,
    offset_x: f64,
    offset_z: f64,
) -> (f64, f64) {
    if placement == "origin" {
        return (1.0, 0.0);
    }
    let side = (users as f64).sqrt().ceil() as usize;
    let base_x = (idx % side) as f64 * spacing;
    let base_z = (idx / side) as f64 * spacing;
    (base_x + offset_x, base_z + offset_z)
}

fn validate(args: &Args) -> Result<(), String> {
    if args.users == 0 {
        return Err("--users must be > 0".to_string());
    }
    if !args.hz.is_finite() || args.hz <= 0.0 || args.hz > 1000.0 {
        return Err("--hz must be in (0, 1000]".to_string());
    }
    if args.duration == 0 {
        return Err("--duration must be > 0".to_string());
    }
    if args.login_concurrency == 0 {
        return Err("--login-concurrency must be > 0".to_string());
    }
    if args.placement != "grid" && args.placement != "origin" {
        return Err("--placement must be one of: grid, origin".to_string());
    }
    if !args.placement_spacing.is_finite() || args.placement_spacing <= 0.0 {
        return Err("--placement-spacing must be finite and > 0".to_string());
    }
    if !args.placement_offset_x.is_finite() || !args.placement_offset_z.is_finite() {
        return Err("--placement-offset-x/z must be finite".to_string());
    }
    if args.server_url.is_empty() {
        return Err("--server-url must not be empty".to_string());
    }
    const SCENARIOS: [&str; 7] = [
        "login",
        "ramp",
        "transform",
        "reconnect",
        "slow-consumer",
        "single-instance",
        "multi-instance",
    ];
    if !SCENARIOS.contains(&args.scenario.as_str()) {
        return Err(format!(
            "--scenario must be one of: {}",
            SCENARIOS.join(", ")
        ));
    }
    if !args.ramp_rate.is_finite() || args.ramp_rate <= 0.0 {
        return Err("--ramp-rate must be > 0".to_string());
    }
    if !args.reconnect_percent.is_finite() || !(0.0..=100.0).contains(&args.reconnect_percent) {
        return Err("--reconnect-percent must be in [0, 100]".to_string());
    }
    if args.reconnect_interval == 0 {
        return Err("--reconnect-interval must be > 0".to_string());
    }
    if !args.slow_consumer_percent.is_finite()
        || !(0.0..=100.0).contains(&args.slow_consumer_percent)
    {
        return Err("--slow-consumer-percent must be in [0, 100]".to_string());
    }
    if args.metrics_interval == 0 {
        return Err("--metrics-interval must be > 0".to_string());
    }
    Ok(())
}

fn resolve_password(args: &Args) -> Result<String, String> {
    if let Some(path) = &args.password_file {
        let raw = std::fs::read_to_string(path)
            .map_err(|e| format!("read password file {}: {e}", path))?;
        let pw = raw.trim_end_matches(['\r', '\n']).to_string();
        if pw.is_empty() {
            return Err("password file is empty".to_string());
        }
        return Ok(pw);
    }
    if !args.password.is_empty() {
        if std::env::var("ORBISYNC_LOAD_PASSWORD").is_err() {
            eprintln!(
                "warning: --password via CLI leaks via process list; prefer ORBISYNC_LOAD_PASSWORD or --password-file"
            );
        }
        return Ok(args.password.clone());
    }
    Err(
        "password not set: use ORBISYNC_LOAD_PASSWORD env var or --password-file (or --password)"
            .to_string(),
    )
}

fn resolve_commit(arg: &str) -> String {
    if !arg.is_empty() {
        return arg.to_string();
    }
    if let Ok(val) = std::env::var("GIT_COMMIT") {
        if !val.is_empty() {
            return val;
        }
    }
    // Try git rev-parse
    if let Ok(out) = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
    {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "unknown".to_string()
}

fn percentile(sorted: &[Duration], p: f64) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    let idx = rank.saturating_sub(1).min(sorted.len() - 1);
    sorted[idx]
}

fn min_max_mean(sorted: &[Duration]) -> (Duration, Duration, Duration) {
    if sorted.is_empty() {
        return (Duration::ZERO, Duration::ZERO, Duration::ZERO);
    }
    let min = sorted[0];
    let max = sorted[sorted.len() - 1];
    let sum: Duration = sorted.iter().sum();
    let mean = sum / (sorted.len() as u32);
    (min, max, mean)
}

fn to_ms(d: Duration) -> f64 {
    d.as_secs_f64() * 1000.0
}

fn percentile_usize(sorted: &[usize], p: f64) -> usize {
    if sorted.is_empty() {
        return 0;
    }
    let rank = (p * sorted.len() as f64).ceil() as usize;
    sorted[rank.saturating_sub(1).min(sorted.len() - 1)]
}

fn duration_stats(sorted: &[Duration]) -> serde_json::Value {
    if sorted.is_empty() {
        return serde_json::json!({ "n": 0 });
    }
    let (_, max, mean) = min_max_mean(sorted);
    serde_json::json!({
        "n": sorted.len(),
        "min_ms": to_ms(sorted[0]),
        "mean_ms": to_ms(mean),
        "p50_ms": to_ms(percentile(sorted, 0.50)),
        "p95_ms": to_ms(percentile(sorted, 0.95)),
        "p99_ms": to_ms(percentile(sorted, 0.99)),
        "max_ms": to_ms(max),
    })
}

fn usize_stats(sorted: &[usize]) -> serde_json::Value {
    if sorted.is_empty() {
        return serde_json::json!({ "n": 0 });
    }
    serde_json::json!({
        "n": sorted.len(),
        "min": sorted[0],
        "p50": percentile_usize(sorted, 0.50),
        "p95": percentile_usize(sorted, 0.95),
        "max": sorted[sorted.len() - 1],
    })
}

fn current_unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

// ---------------------------------------------------------------------------
// HTTP flows
// ---------------------------------------------------------------------------

async fn http_login(
    client: &reqwest::Client,
    server_url: &str,
    login_id: &str,
    password: &str,
) -> Result<String, String> {
    // Retry on 429 (argon2 rate limit) with backoff — real server will throttle
    // concurrent logins, but we should not count it as permanent failure.
    let mut attempt = 0;
    loop {
        let url = format!("{}/v1/auth/login", http_origin(server_url));
        let body = LoginRequest {
            login_id: login_id.to_string(),
            password: password.to_string(),
        };
        let resp = client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("login request failed: {e}"))?;
        if resp.status().is_success() {
            let parsed: LoginResponse = resp
                .json()
                .await
                .map_err(|e| format!("login decode failed: {e}"))?;
            if parsed.access_token.is_empty() {
                return Err("login returned empty access_token".to_string());
            }
            return Ok(parsed.access_token);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let err = format!("login {status}: {text}");
        if (status.as_u16() == 429 || text.contains("RATE_LIMITED")) && attempt < 5 {
            let backoff = Duration::from_millis(100 * (1 << attempt));
            tokio::time::sleep(backoff).await;
            attempt += 1;
            continue;
        }
        return Err(err);
    }
}

async fn provision_users(
    client: &reqwest::Client,
    server_url: &str,
    admin_token: &str,
    count: usize,
) -> Result<Vec<VirtualUserCredential>, String> {
    let url = format!("{}/v1/users", http_origin(server_url));
    let mut credentials = Vec::with_capacity(count);
    for index in 0..count {
        let body = serde_json::json!({
            "login_id": format!("load-generator-{}-{index}", uuid::Uuid::now_v7()),
            "display_name": format!("Load generator user {index}"),
        });
        let response = client
            .post(&url)
            .header("authorization", format!("Bearer {admin_token}"))
            .header("accept", "application/vnd.orbisync.user-credential+json")
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("create load user failed: {e}"))?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(format!("create load user {status}: {text}"));
        }
        let created: CreatedUserResponse = response
            .json()
            .await
            .map_err(|e| format!("decode load user response failed: {e}"))?;
        credentials.push(VirtualUserCredential {
            user_id: created.user.id,
            login_id: created.user.login_id,
            password: created.temporary_password,
        });
    }
    Ok(credentials)
}

async fn provision_load_role(
    client: &reqwest::Client,
    server_url: &str,
    admin_token: &str,
) -> Result<String, String> {
    let url = format!("{}/v1/roles", http_origin(server_url));
    let body = serde_json::json!({
        "name": format!("Load Generator Entity Writer {}", uuid::Uuid::now_v7()),
        "description": "Permissions for load-generator entities",
        "permissions": ["entity.spawn", "entity.update.own"],
    });
    let response = client
        .post(&url)
        .header("authorization", format!("Bearer {admin_token}"))
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("create load-generator role failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("create load-generator role {status}: {text}"));
    }
    let role: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("decode load-generator role failed: {e}"))?;
    role.get("id")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "load-generator role response missing id".to_string())
}

async fn assign_load_role(
    client: &reqwest::Client,
    server_url: &str,
    admin_token: &str,
    user_id: &str,
    role_id: &str,
) -> Result<(), String> {
    let url = format!("{}/v1/users/{user_id}/roles", http_origin(server_url));
    let response = client
        .put(&url)
        .header("authorization", format!("Bearer {admin_token}"))
        .json(&serde_json::json!({ "role_ids": [role_id] }))
        .send()
        .await
        .map_err(|e| format!("assign load-generator role failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(format!("assign load-generator role {status}: {text}"));
    }
    Ok(())
}

async fn limited_login(
    login_gate: &Semaphore,
    client: &reqwest::Client,
    server_url: &str,
    login_id: &str,
    password: &str,
) -> Result<String, String> {
    let _permit = login_gate
        .acquire()
        .await
        .map_err(|_| "login concurrency gate is closed".to_string())?;
    http_login(client, server_url, login_id, password).await
}

async fn http_ticket(
    client: &reqwest::Client,
    server_url: &str,
    access_token: &str,
) -> Result<String, String> {
    let mut attempt = 0;
    loop {
        let url = format!("{}/v1/realtime/tickets", http_origin(server_url));
        let resp = client
            .post(&url)
            .header("authorization", format!("Bearer {access_token}"))
            .header("content-length", "0")
            .send()
            .await
            .map_err(|e| format!("ticket request failed: {e}"))?;
        if resp.status().is_success() {
            let parsed: TicketResponse = resp
                .json()
                .await
                .map_err(|e| format!("ticket decode failed: {e}"))?;
            let t = parsed
                .ticket()
                .ok_or("ticket missing in response")?
                .to_string();
            if t.is_empty() {
                return Err("ticket empty".to_string());
            }
            return Ok(t);
        }
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        let err = format!("ticket {status}: {text}");
        if (status.as_u16() == 429 || text.contains("RATE_LIMITED")) && attempt < 5 {
            let backoff = Duration::from_millis(100 * (1 << attempt));
            tokio::time::sleep(backoff).await;
            attempt += 1;
            continue;
        }
        return Err(err);
    }
}

async fn ensure_instance(
    client: &reqwest::Client,
    server_url: &str,
    access_token: &str,
    instance_id: &str,
) -> Result<String, String> {
    if !instance_id.is_empty() {
        return Ok(instance_id.to_string());
    }
    // Create world then instance via public REST.
    let world_url = format!("{}/v1/worlds", http_origin(server_url));
    let world_body = serde_json::json!({"name": format!("load-world-{}", uuid::Uuid::now_v7()), "capacity": 1000});
    let resp = client
        .post(&world_url)
        .header("authorization", format!("Bearer {access_token}"))
        .json(&world_body)
        .send()
        .await
        .map_err(|e| format!("create world failed: {e}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(format!("create world {s}: {t}"));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("world decode: {e}"))?;
    let world_id = v
        .get("id")
        .and_then(|x| x.as_str())
        .ok_or("world id missing")?
        .to_string();
    let inst_url = format!("{}/v1/instances", http_origin(server_url));
    let inst_body = serde_json::json!({"world_id": world_id});
    let resp = client
        .post(&inst_url)
        .header("authorization", format!("Bearer {access_token}"))
        .json(&inst_body)
        .send()
        .await
        .map_err(|e| format!("create instance failed: {e}"))?;
    if !resp.status().is_success() {
        let s = resp.status();
        let t = resp.text().await.unwrap_or_default();
        return Err(format!("create instance {s}: {t}"));
    }
    let v: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("instance decode: {e}"))?;
    let iid = v
        .get("id")
        .and_then(|x| x.as_str())
        .ok_or("instance id missing")?
        .to_string();
    Ok(iid)
}

// ---------------------------------------------------------------------------
// WebSocket helpers
// ---------------------------------------------------------------------------

fn encode_envelope(env: &Envelope) -> Vec<u8> {
    let mut b = Vec::new();
    env.encode(&mut b).expect("encode");
    b
}

fn decode_envelope(bytes: &[u8]) -> Option<Envelope> {
    Envelope::decode(bytes).ok()
}

async fn ws_connect(
    ws_url: &str,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>,
    String,
> {
    let uri: tungstenite::http::Uri = ws_url.parse().map_err(|e| format!("ws uri parse: {e}"))?;
    let req = tungstenite::client::ClientRequestBuilder::new(uri)
        .with_sub_protocol(WEBSOCKET_SUBPROTOCOL);
    let (ws, _) = tokio_tungstenite::connect_async(req)
        .await
        .map_err(|e| format!("ws connect {ws_url}: {e}"))?;
    Ok(ws)
}

async fn send_binary<S>(
    ws: &mut S,
    bytes: Vec<u8>,
    sent_bytes: &mut u64,
) -> Result<(), tungstenite::Error>
where
    S: Sink<tungstenite::Message, Error = tungstenite::Error> + Unpin,
{
    *sent_bytes += bytes.len() as u64;
    ws.send(tungstenite::Message::Binary(bytes.into())).await
}

// Receive next non-heartbeat envelope with timeout.
async fn recv_envelope<S>(
    ws: &mut S,
    timeout: Duration,
    received_bytes: &mut u64,
) -> Option<Envelope>
where
    S: StreamExt<Item = Result<tungstenite::Message, tungstenite::Error>> + Unpin,
{
    let fut = async {
        loop {
            let msg = ws.next().await?;
            match msg {
                Ok(tungstenite::Message::Binary(data)) => {
                    *received_bytes += data.len() as u64;
                    if let Some(env) = decode_envelope(&data) {
                        match &env.payload {
                            Some(envelope::Payload::Error(_)) => {
                                return Some(env);
                            }
                            _ => {
                                // Skip heartbeat-ish but we don't have heartbeat payload defined as separate tag 29 etc.
                                // Our minimal proto only includes up to 26+34, so heartbeat is unknown and will be skipped as None.
                                // To be safe, if payload is None (unknown field), skip? Actually unknown payload is None.
                                // For heartbeat, decode would produce None payload because tag 29 not in our oneof.
                                // So we skip those.
                                if env.payload.is_some() {
                                    return Some(env);
                                }
                                // else it's heartbeat-like, skip
                                continue;
                            }
                        }
                    }
                }
                Ok(tungstenite::Message::Close(_)) => return None,
                Ok(_) => continue,
                Err(_) => return None,
            }
        }
    };
    tokio::time::timeout(timeout, fut).await.ok().flatten()
}

// ---------------------------------------------------------------------------
// Per-user driver (real network)
// ---------------------------------------------------------------------------

fn finalize_result(mut result: UserResult) -> UserResult {
    if result.ws_connected && result.disconnects.is_empty() {
        let reason = if result.join_ok { "normal" } else { "error" };
        result.disconnects.insert(reason.to_string(), 1);
    }
    result
}

fn merge_result(target: &mut UserResult, extra: UserResult) {
    target.login_ok |= extra.login_ok;
    target.ticket_ok |= extra.ticket_ok;
    target.ws_connected |= extra.ws_connected;
    target.hello_ok |= extra.hello_ok;
    target.join_ok |= extra.join_ok;
    if target.join_latency.is_none() {
        target.join_latency = extra.join_latency;
    }
    target.transform_rtts.extend(extra.transform_rtts);
    target.messages_received += extra.messages_received;
    target.errors += extra.errors;
    target.dropped_updates += extra.dropped_updates;
    target.sent_bytes += extra.sent_bytes;
    target.received_bytes += extra.received_bytes;
    target.snapshot_sizes.extend(extra.snapshot_sizes);
    target.reconnects += extra.reconnects;
    if target.ramp_stage.is_none() {
        target.ramp_stage = extra.ramp_stage;
        target.ramp_start_delay_ms = extra.ramp_start_delay_ms;
    }
    for (reason, count) in extra.disconnects {
        *target.disconnects.entry(reason).or_insert(0) += count;
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_virtual_user(
    idx: usize,
    args: Arc<Args>,
    http_client: reqwest::Client,
    instance_id: String,
    ticks_per_user: usize,
    tick_interval: Duration,
    login_id: String,
    password: String,
    start_delay: Duration,
    login_gate: Arc<Semaphore>,
) -> UserResult {
    if !start_delay.is_zero() {
        tokio::time::sleep(start_delay).await;
    }
    if args.scenario == "login" {
        let mut result = UserResult::default();
        match limited_login(
            &login_gate,
            &http_client,
            &args.server_url,
            &login_id,
            &password,
        )
        .await
        {
            Ok(_) => result.login_ok = true,
            Err(error) => {
                eprintln!("[user {idx}] login failed: {error}");
                result.errors += 1;
            }
        }
        return result;
    }
    let mut result = finalize_result(
        run_virtual_user_once(
            idx,
            Arc::clone(&args),
            http_client.clone(),
            instance_id.clone(),
            ticks_per_user,
            tick_interval,
            login_id.clone(),
            password.clone(),
            Arc::clone(&login_gate),
        )
        .await,
    );
    if args.scenario == "ramp" {
        result.ramp_stage = Some(idx + 1);
        result.ramp_start_delay_ms = Some(start_delay.as_millis());
    }
    let reconnect_selected = args.scenario == "reconnect"
        && (idx as f64) < (args.users as f64 * args.reconnect_percent / 100.0).ceil();
    if reconnect_selected && result.join_ok {
        // The first run closes its socket at the test boundary. Open a fresh
        // public-contract session for the selected users so the reconnect is
        // real (not a counter-only simulation). The reconnect interval remains
        // part of the recorded parameters; a short second session keeps the
        // seven-scenario smoke runs bounded.
        let reconnect_args = Arc::new(Args {
            scenario: "transform".to_string(),
            ..(*args).clone()
        });
        let reconnect_ticks = 1.max((args.hz * args.reconnect_interval.min(1) as f64) as usize);
        let reconnect = finalize_result(
            run_virtual_user_once(
                idx,
                reconnect_args,
                http_client,
                instance_id,
                reconnect_ticks,
                tick_interval,
                login_id,
                password,
                login_gate,
            )
            .await,
        );
        result.reconnects += 1;
        merge_result(&mut result, reconnect);
    }
    result
}

#[allow(clippy::too_many_arguments)]
async fn run_virtual_user_once(
    idx: usize,
    args: Arc<Args>,
    http_client: reqwest::Client,
    instance_id: String,
    ticks_per_user: usize,
    tick_interval: Duration,
    login_id: String,
    password: String,
    login_gate: Arc<Semaphore>,
) -> UserResult {
    let mut res = UserResult::default();
    // 1. login
    let token = match limited_login(
        &login_gate,
        &http_client,
        &args.server_url,
        &login_id,
        &password,
    )
    .await
    {
        Ok(t) => {
            res.login_ok = true;
            t
        }
        Err(e) => {
            eprintln!("[user {idx}] login failed: {e}");
            res.errors += 1;
            return res;
        }
    };
    // 2. ticket
    let ticket = match http_ticket(&http_client, &args.server_url, &token).await {
        Ok(t) => {
            res.ticket_ok = true;
            t
        }
        Err(e) => {
            eprintln!("[user {idx}] ticket failed: {e}");
            res.errors += 1;
            return res;
        }
    };
    // 3. ws connect
    let ws_url = ws_url_from(&args.server_url, &args.ws_url);
    let mut ws = match ws_connect(&ws_url).await {
        Ok(w) => {
            res.ws_connected = true;
            w
        }
        Err(e) => {
            eprintln!("[user {idx}] ws connect failed: {e}");
            res.errors += 1;
            return res;
        }
    };
    // 4. ClientHello
    let hello = ClientHello {
        supported_minor_min: 0,
        supported_minor_max: 0,
        realtime_ticket: ticket,
        client_name: "load-generator".to_string(),
        client_version: env!("CARGO_PKG_VERSION").to_string(),
        client_type: "load".to_string(),
        supported_compressions: Vec::new(),
        supported_features: Vec::new(),
        resume_token: String::new(),
    };
    let env_hello = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: 0,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 1,
        sent_at_unix_ms: current_unix_ms(),
        instance_id: String::new(),
        payload: Some(envelope::Payload::ClientHello(hello)),
    };
    if send_binary(&mut ws, encode_envelope(&env_hello), &mut res.sent_bytes)
        .await
        .is_err()
    {
        eprintln!("[user {idx}] send hello failed");
        res.errors += 1;
        return res;
    }
    let hello_resp = recv_envelope(&mut ws, Duration::from_secs(5), &mut res.received_bytes).await;
    let negotiated_minor = match hello_resp {
        Some(env) => match env.payload {
            Some(envelope::Payload::ServerHello(h)) => {
                res.hello_ok = true;
                h.negotiated_minor
            }
            Some(envelope::Payload::Error(e)) => {
                eprintln!("[user {idx}] hello error {}: {}", e.code, e.message);
                res.errors += 1;
                return res;
            }
            _ => {
                eprintln!("[user {idx}] unexpected hello response");
                res.errors += 1;
                return res;
            }
        },
        None => {
            eprintln!("[user {idx}] hello timeout");
            res.errors += 1;
            return res;
        }
    };
    // 5. Join
    let join = JoinInstance {
        world_instance_id: instance_id.clone(),
    };
    let env_join = Envelope {
        protocol_major: PROTOCOL_MAJOR,
        protocol_minor: negotiated_minor,
        message_id: uuid::Uuid::now_v7().to_string(),
        sequence: 2,
        sent_at_unix_ms: current_unix_ms(),
        instance_id: String::new(),
        payload: Some(envelope::Payload::JoinInstance(join)),
    };
    let join_start = Instant::now();
    if send_binary(&mut ws, encode_envelope(&env_join), &mut res.sent_bytes)
        .await
        .is_err()
    {
        eprintln!("[user {idx}] send join failed");
        res.errors += 1;
        return res;
    }
    // Expect JoinAccepted + Snapshot (order JoinAccepted then Snapshot)
    let mut got_accepted = false;
    let mut snapshot_chunks: HashMap<String, (u32, BTreeMap<u32, usize>)> = HashMap::new();
    let mut join_done = false;
    for _ in 0..1024 {
        if let Some(env) =
            recv_envelope(&mut ws, Duration::from_secs(5), &mut res.received_bytes).await
        {
            match &env.payload {
                Some(envelope::Payload::JoinAccepted(_)) => {
                    got_accepted = true;
                    if got_accepted && !res.snapshot_sizes.is_empty() {
                        join_done = true;
                        break;
                    }
                }
                Some(envelope::Payload::Snapshot(snapshot)) => {
                    let entry = snapshot_chunks
                        .entry(snapshot.snapshot_id.clone())
                        .or_insert_with(|| (snapshot.chunk_count, BTreeMap::new()));
                    entry.1.insert(snapshot.chunk_index, snapshot.data.len());
                    if entry.0 > 0 && entry.1.len() == entry.0 as usize {
                        let size = entry.1.values().sum();
                        res.snapshot_sizes.push(size);
                    }
                    if got_accepted && !res.snapshot_sizes.is_empty() {
                        join_done = true;
                        break;
                    }
                }
                Some(envelope::Payload::Error(e)) => {
                    eprintln!("[user {idx}] join error {}: {}", e.code, e.message);
                    res.errors += 1;
                    return res;
                }
                _ => continue,
            }
            if got_accepted && !res.snapshot_sizes.is_empty() {
                join_done = true;
                break;
            }
        } else {
            break;
        }
    }
    if !join_done || !got_accepted {
        eprintln!("[user {idx}] join timeout/incomplete");
        res.errors += 1;
        return res;
    }
    res.join_ok = true;
    res.join_latency = Some(join_start.elapsed());

    // 6. Transform loop at Hz
    let entity_id = uuid::Uuid::now_v7().to_string();
    let mut expected_revision: u64 = 1;
    let (mut pos_x, mut pos_z) = initial_position(
        &args.placement,
        idx,
        args.users,
        args.placement_spacing,
        args.placement_offset_x,
        args.placement_offset_z,
    );
    let slow_consumer = args.scenario == "slow-consumer"
        && (idx as f64) < (args.users as f64 * args.slow_consumer_percent / 100.0).ceil();
    for tick in 0..ticks_per_user {
        if tick > 0 {
            tokio::time::sleep(tick_interval).await;
        }
        // Small movement to keep speed <50 m/s and distance per tick <10 m.
        // No wrap — wrapping 20→1 would be a 19 m teleport exceeding MAX_TICK_DISTANCE.
        pos_x += 0.5;
        pos_z += 0.5;
        let transform = Transform {
            position_x: pos_x as f32,
            position_y: 0.0,
            position_z: pos_z as f32,
            rotation_x: 0.0,
            rotation_y: 0.0,
            rotation_z: 0.0,
            rotation_w: 1.0,
        };
        let input = TransformInput {
            entity_id: entity_id.clone(),
            expected_revision,
            transform: Some(transform),
        };
        let env_t = Envelope {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: negotiated_minor,
            message_id: uuid::Uuid::now_v7().to_string(),
            sequence: 10 + tick as u64,
            sent_at_unix_ms: current_unix_ms(),
            instance_id: instance_id.clone(),
            payload: Some(envelope::Payload::TransformInput(input)),
        };
        let send_at = Instant::now();
        if send_binary(&mut ws, encode_envelope(&env_t), &mut res.sent_bytes)
            .await
            .is_err()
        {
            eprintln!("[user {idx}] send transform tick {tick} failed");
            res.errors += 1;
            continue;
        }
        if slow_consumer {
            // Deliberately do not read while the server is producing updates.
            // This exercises the server-side mailbox/socket backpressure instead
            // of merely sleeping between otherwise normal request/response pairs.
            tokio::time::sleep(Duration::from_millis(args.slow_consumer_delay_ms)).await;
            res.dropped_updates += 1;
            continue;
        }
        // Wait for StateDelta containing our entity
        let mut found = false;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            let remain = deadline.saturating_duration_since(Instant::now());
            if let Some(env) = recv_envelope(&mut ws, remain, &mut res.received_bytes).await {
                match env.payload {
                    Some(envelope::Payload::StateDelta(delta)) => {
                        res.messages_received += 1;
                        if let Some(es) = delta.entities.iter().find(|e| e.entity_id == entity_id) {
                            let rtt = send_at.elapsed();
                            res.transform_rtts.push(rtt);
                            expected_revision = es.revision;
                            found = true;
                            break;
                        }
                        // delta for other entity — still count but keep waiting for ours
                    }
                    Some(envelope::Payload::Error(e)) => {
                        eprintln!("[user {idx}] transform error {}: {}", e.code, e.message);
                        res.errors += 1;
                        break;
                    }
                    _ => {
                        // count other messages as received
                        res.messages_received += 1;
                    }
                }
            } else {
                break;
            }
        }
        if !found {
            // Distinguish real failure: no delta for our entity within deadline is a dropped update
            res.dropped_updates += 1;
            res.errors += 1;
        }
    }
    if slow_consumer {
        // Drain only briefly so the report includes actual received frame bytes,
        // while preserving the intentional slow-consumer behavior.
        let deadline = Instant::now() + Duration::from_millis(250);
        while Instant::now() < deadline {
            let remain = deadline.saturating_duration_since(Instant::now());
            let Some(_) = recv_envelope(&mut ws, remain, &mut res.received_bytes).await else {
                break;
            };
            res.messages_received += 1;
        }
    }
    let _ = ws.close(None).await;
    res
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    let args = Arc::new(Args::parse());
    if let Err(msg) = validate(&args) {
        eprintln!("error: {msg}");
        std::process::exit(2);
    }

    let commit = resolve_commit(&args.commit);
    let ticks_per_user = if args.scenario == "login" {
        0
    } else {
        (args.duration as f64 * args.hz).round() as usize
    };
    let tick_interval = Duration::from_secs_f64(1.0 / args.hz);
    let total_ops_expected = ticks_per_user * args.users;

    println!("OrbiSync load-generator report (real network)");
    println!("  server_url: {}", args.server_url);
    println!(
        "  ws_url:     {}",
        ws_url_from(&args.server_url, &args.ws_url)
    );
    println!("  commit:     {}", commit);
    println!("  users:      {}", args.users);
    println!("  scenario:   {}", args.scenario);
    println!("  hz:         {:.2}", args.hz);
    println!("  duration:   {}s", args.duration);
    println!("  ticks_per_user: {}", ticks_per_user);
    println!("  total_ops_expected: {}", total_ops_expected);
    println!();

    let http_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .expect("client");

    let password = match resolve_password(&args) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(2);
        }
    };

    // Login once to ensure credentials work and to create world/instance if needed
    let token_for_setup = match http_login(
        &http_client,
        &args.server_url,
        &args.login_id,
        &password,
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            eprintln!("setup login failed: {e}");
            eprintln!(
                "hint: ensure the server is running and login_id/password are valid. For a fresh DB, bootstrap an admin first."
            );
            std::process::exit(1);
        }
    };
    let instance_id = if args.no_auto_create && args.instance_id.is_empty() {
        eprintln!("--no-auto-create requires --instance-id");
        std::process::exit(2);
    } else {
        match ensure_instance(
            &http_client,
            &args.server_url,
            &token_for_setup,
            &args.instance_id,
        )
        .await
        {
            Ok(id) => {
                println!("  instance_id: {}", id);
                println!();
                id
            }
            Err(e) => {
                eprintln!("could not ensure instance: {e}");
                std::process::exit(1);
            }
        }
    };

    let load_role_id = match provision_load_role(&http_client, &args.server_url, &token_for_setup).await {
        Ok(role_id) => role_id,
        Err(error) => {
            eprintln!("could not provision load-generator role: {error}");
            std::process::exit(1);
        }
    };
    let virtual_users = match provision_users(
        &http_client,
        &args.server_url,
        &token_for_setup,
        args.users,
    )
    .await
    {
        Ok(users) => users,
        Err(error) => {
            eprintln!("could not provision load-generator users: {error}");
            std::process::exit(1);
        }
    };
    for user in &virtual_users {
        if let Err(error) = assign_load_role(
            &http_client,
            &args.server_url,
            &token_for_setup,
            &user.user_id,
            &load_role_id,
        )
        .await
        {
            eprintln!("could not assign load-generator role: {error}");
            std::process::exit(1);
        }
    }
    println!("  provisioned_users: {}", virtual_users.len());
    println!();

    let instance_ids = if args.scenario == "multi-instance" {
        if args.no_auto_create {
            return eprintln!(
                "multi-instance requires auto-created instances; omit --no-auto-create"
            );
        }
        let mut ids = Vec::with_capacity(args.users);
        for _ in 0..args.users {
            match ensure_instance(&http_client, &args.server_url, &token_for_setup, "").await {
                Ok(id) => ids.push(id),
                Err(error) => {
                    eprintln!("could not ensure multi-instance target: {error}");
                    std::process::exit(1);
                }
            }
        }
        ids
    } else {
        vec![instance_id.clone(); args.users]
    };

    let start_wall = Instant::now();
    let started_at_unix_ms = current_unix_ms();
    let metrics = Arc::new(Mutex::new(MetricsCollector::new(
        args.metrics_url.clone(),
        args.metrics_interval,
        start_wall,
    )));
    scrape_metrics(&http_client, &metrics).await;
    let (metrics_stop_tx, mut metrics_stop_rx) = tokio::sync::watch::channel(false);
    let metrics_task = if args.metrics_url.is_some() {
        let metrics = Arc::clone(&metrics);
        let client = http_client.clone();
        let interval = Duration::from_secs(args.metrics_interval);
        Some(tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.tick().await;
            loop {
                tokio::select! {
                    _ = ticker.tick() => scrape_metrics(&client, &metrics).await,
                    changed = metrics_stop_rx.changed() => {
                        if changed.is_err() || *metrics_stop_rx.borrow() { break; }
                    }
                }
            }
        }))
    } else {
        None
    };
    let results: Arc<Mutex<Vec<UserResult>>> = Arc::new(Mutex::new(Vec::new()));
    let mut handles = Vec::new();
    let ramp_scenario = args.scenario == "ramp";
    let ramp_rate = args.ramp_rate;
    let login_gate = Arc::new(Semaphore::new(args.login_concurrency));
    for (i, instance_id) in instance_ids.iter().enumerate() {
        let credential = virtual_users[i].clone();
        let args_c = Arc::clone(&args);
        let client_c = http_client.clone();
        let iid_c = instance_id.clone();
        let login_id_c = credential.login_id;
        let pw_c = credential.password;
        let res_c = Arc::clone(&results);
        let login_gate_c = Arc::clone(&login_gate);
        handles.push(tokio::spawn(async move {
            let r = run_virtual_user(
                i,
                args_c,
                client_c,
                iid_c,
                ticks_per_user,
                tick_interval,
                login_id_c,
                pw_c,
                if ramp_scenario {
                    Duration::from_secs_f64(i as f64 / ramp_rate)
                } else {
                    Duration::ZERO
                },
                login_gate_c,
            )
            .await;
            res_c.lock().await.push(r);
        }));
    }
    for h in handles {
        let _ = h.await;
    }
    let wall_elapsed = start_wall.elapsed();
    let ended_at_unix_ms = current_unix_ms();
    let _ = metrics_stop_tx.send(true);
    if let Some(task) = metrics_task {
        let _ = task.await;
    }
    scrape_metrics(&http_client, &metrics).await;

    let guard = results.lock().await;
    let total_users = guard.len();
    let login_ok = guard.iter().filter(|r| r.login_ok).count();
    let ticket_ok = guard.iter().filter(|r| r.ticket_ok).count();
    let ws_ok = guard.iter().filter(|r| r.ws_connected).count();
    let hello_ok = guard.iter().filter(|r| r.hello_ok).count();
    let join_ok = guard.iter().filter(|r| r.join_ok).count();
    let join_fail = total_users.saturating_sub(join_ok);
    let mut join_latencies: Vec<Duration> = guard.iter().filter_map(|r| r.join_latency).collect();
    join_latencies.sort_unstable();
    let mut rtts: Vec<Duration> = guard
        .iter()
        .flat_map(|r| r.transform_rtts.clone())
        .collect();
    rtts.sort_unstable();
    let messages_received: usize = guard.iter().map(|r| r.messages_received).sum();
    let errors: usize = guard.iter().map(|r| r.errors).sum();
    let dropped_updates: usize = guard.iter().map(|r| r.dropped_updates).sum();
    let sent_bytes: u64 = guard.iter().map(|r| r.sent_bytes).sum();
    let received_bytes: u64 = guard.iter().map(|r| r.received_bytes).sum();
    let mut snapshot_sizes: Vec<usize> = guard
        .iter()
        .flat_map(|r| r.snapshot_sizes.iter().copied())
        .collect();
    snapshot_sizes.sort_unstable();
    let mut disconnects = BTreeMap::new();
    for result in guard.iter() {
        for (reason, count) in &result.disconnects {
            *disconnects.entry(reason.clone()).or_insert(0usize) += count;
        }
    }
    let reconnects: usize = guard.iter().map(|r| r.reconnects).sum();
    let rtt_samples = rtts.len();
    let join_samples = join_latencies.len();

    let (j_min, j_max, j_mean) = min_max_mean(&join_latencies);
    let j_p50 = percentile(&join_latencies, 0.50);
    let j_p95 = percentile(&join_latencies, 0.95);
    let j_p99 = percentile(&join_latencies, 0.99);

    let (r_min, r_max, r_mean) = min_max_mean(&rtts);
    let r_p50 = percentile(&rtts, 0.50);
    let r_p95 = percentile(&rtts, 0.95);
    let r_p99 = percentile(&rtts, 0.99);
    let bytes_per_second = wall_elapsed.as_secs_f64().max(f64::EPSILON);

    println!(
        "Results (wall {:.2}s, requested {}s):",
        wall_elapsed.as_secs_f64(),
        args.duration
    );
    println!(
        "  login_ok:   {}/{} (fail {})",
        login_ok,
        total_users,
        total_users - login_ok
    );
    println!(
        "  ticket_ok:  {}/{} (fail {})",
        ticket_ok,
        total_users,
        total_users - ticket_ok
    );
    println!(
        "  ws_connected: {}/{} (fail {})",
        ws_ok,
        total_users,
        total_users - ws_ok
    );
    println!(
        "  hello_ok:   {}/{} (fail {})",
        hello_ok,
        total_users,
        total_users - hello_ok
    );
    println!(
        "  join_ok:    {}/{} (fail {})",
        join_ok, total_users, join_fail
    );
    if join_samples > 0 {
        println!(
            "  join latency (n={}): min {:.2} ms  mean {:.2} ms  p50 {:.2} ms  p95 {:.2} ms  p99 {:.2} ms  max {:.2} ms",
            join_samples,
            to_ms(j_min),
            to_ms(j_mean),
            to_ms(j_p50),
            to_ms(j_p95),
            to_ms(j_p99),
            to_ms(j_max)
        );
    } else {
        println!("  join latency: n=0 (no successful joins)");
    }
    println!(
        "  transform expected: {} ({} users × {} ticks)",
        total_ops_expected, args.users, ticks_per_user
    );
    println!("  transform rtt samples: {} (received)", rtt_samples);
    println!("  messages_received (all deltas): {}", messages_received);
    println!("  errors (timeouts + ws + http): {}", errors);
    println!("  dropped updates: {}", dropped_updates);
    println!(
        "  network: sent_bytes={} received_bytes={} sent_bytes_per_second={:.2} received_bytes_per_second={:.2}",
        sent_bytes,
        received_bytes,
        sent_bytes as f64 / bytes_per_second,
        received_bytes as f64 / bytes_per_second
    );
    println!(
        "  disconnects: {:?} reconnects: {}",
        disconnects, reconnects
    );
    if snapshot_sizes.is_empty() {
        println!("  snapshot size: n=0 (no complete logical Snapshot observed)");
    } else {
        println!(
            "  snapshot size (logical reassembled bytes, n={}): min {} B p50 {} B p95 {} B max {} B",
            snapshot_sizes.len(),
            snapshot_sizes[0],
            percentile_usize(&snapshot_sizes, 0.50),
            percentile_usize(&snapshot_sizes, 0.95),
            snapshot_sizes[snapshot_sizes.len() - 1]
        );
    }
    if rtt_samples > 0 {
        println!(
            "  rtt (n={}): min {:.2} ms  mean {:.2} ms  p50 {:.2} ms  p95 {:.2} ms  p99 {:.2} ms  max {:.2} ms",
            rtt_samples,
            to_ms(r_min),
            to_ms(r_mean),
            to_ms(r_p50),
            to_ms(r_p95),
            to_ms(r_p99),
            to_ms(r_max)
        );
    } else {
        println!("  rtt: n=0 (no transform round-trips observed)");
    }
    let metrics_report = metrics.lock().await.report.clone();
    println!(
        "  server metrics: {} (samples={}, unavailable values are explicit)",
        if metrics_report.configured {
            "scrape configured"
        } else {
            "server-side metrics unavailable"
        },
        metrics_report.samples.len()
    );
    let outbound_depth_values: Vec<f64> = metrics_report
        .samples
        .iter()
        .flat_map(|sample| {
            sample
                .values
                .iter()
                .filter(|(name, _)| metric_name(name) == "outbound_queue_depth")
                .map(|(_, value)| *value)
        })
        .collect();
    let outbound_depth_max_values: Vec<f64> = metrics_report
        .samples
        .iter()
        .flat_map(|sample| {
            sample
                .values
                .iter()
                .filter(|(name, _)| metric_name(name) == "outbound_queue_depth_max")
                .map(|(_, value)| *value)
        })
        .collect();
    let outbound_bytes_max_values: Vec<f64> = metrics_report
        .samples
        .iter()
        .flat_map(|sample| {
            sample
                .values
                .iter()
                .filter(|(name, _)| metric_name(name) == "outbound_queue_bytes_max")
                .map(|(_, value)| *value)
        })
        .collect();
    if outbound_depth_values.is_empty() && outbound_depth_max_values.is_empty() {
        println!("  outbound queue: unavailable (outbound_queue_depth/_max was not scraped)");
    } else {
        println!(
            "  outbound queue: observed depth_samples={} depth_max_samples={} depth_max={:.2} bytes_max={:.0}",
            outbound_depth_values.len(),
            outbound_depth_max_values.len(),
            outbound_depth_max_values
                .iter()
                .copied()
                .fold(0.0, f64::max),
            outbound_bytes_max_values
                .iter()
                .copied()
                .fold(0.0, f64::max)
        );
    }
    let rss_values: Vec<f64> = metrics_report
        .samples
        .iter()
        .flat_map(|sample| {
            sample
                .values
                .iter()
                .filter(|(name, _)| metric_name(name) == "process_resident_memory_bytes")
                .map(|(_, value)| *value)
        })
        .collect();
    let ramp_stage_results: Vec<&UserResult> = guard
        .iter()
        .filter(|result| result.ramp_stage.is_some())
        .collect();
    let ramp_baseline_ms = ramp_stage_results
        .iter()
        .filter_map(|result| result.join_latency.map(to_ms))
        .min_by(|a, b| a.total_cmp(b));
    let ramp_degradation_stage = if args.scenario == "ramp" {
        ramp_stage_results
            .iter()
            .filter_map(|result| {
                let latency_ms = result.join_latency.map(to_ms)?;
                let baseline = ramp_baseline_ms?;
                (latency_ms > baseline * 2.0).then_some(result.ramp_stage?)
            })
            .min()
    } else {
        None
    };
    if args.scenario == "ramp" {
        let rss_summary = rss_values
            .iter()
            .copied()
            .reduce(f64::max)
            .map(|value| format!("{value:.0} B max"))
            .unwrap_or_else(|| "unavailable".to_string());
        let queue_summary = outbound_depth_max_values
            .iter()
            .copied()
            .reduce(f64::max)
            .map(|value| format!("{value:.2} max"))
            .unwrap_or_else(|| "unavailable".to_string());
        println!(
            "  ramp stages: {} (latency/queue/RSS recorded per stage; queue={} RSS={})",
            ramp_stage_results.len(),
            queue_summary,
            rss_summary
        );
        println!(
            "  degradation onset: {}",
            ramp_degradation_stage
                .map(|stage| format!("stage {} (join latency > 2x best observed stage)", stage))
                .unwrap_or_else(|| {
                    "unavailable (no stage exceeded the explicit 2x latency threshold)".to_string()
                })
        );
    }
    println!(
        "  test window: started_at_unix_ms={} ended_at_unix_ms={}",
        started_at_unix_ms, ended_at_unix_ms
    );
    let ramp_stages: Vec<serde_json::Value> = guard
        .iter()
        .filter_map(|result| {
            Some(serde_json::json!({
                "stage": result.ramp_stage?,
                "connections_started": result.ramp_stage,
                "start_delay_ms": result.ramp_start_delay_ms,
                "join_latency_ms": result.join_latency.map(to_ms),
                "queue_depth": if outbound_depth_max_values.is_empty() {
                    serde_json::Value::String("unavailable".to_string())
                } else {
                    serde_json::json!(outbound_depth_max_values.iter().copied().fold(0.0, f64::max))
                },
                "rss_bytes": if rss_values.is_empty() {
                    serde_json::Value::String("unavailable".to_string())
                } else {
                    serde_json::json!(rss_values.iter().copied().fold(0.0, f64::max))
                },
            }))
        })
        .collect();

    let report = serde_json::json!({
        "tool": "orbisync-load-generator",
        "server_url": args.server_url,
        "ws_url": ws_url_from(&args.server_url, &args.ws_url),
        "commit": commit,
        "scenario": args.scenario,
        "users": args.users,
        "placement": args.placement,
        "placement_spacing": args.placement_spacing,
        "placement_offset_x": args.placement_offset_x,
        "placement_offset_z": args.placement_offset_z,
        "hz": args.hz,
        "duration_seconds": args.duration,
        "login_concurrency": args.login_concurrency,
        "ticks_per_user": ticks_per_user,
        "total_ops_expected": total_ops_expected,
        "instance_ids": instance_ids,
        "started_at_unix_ms": started_at_unix_ms,
        "ended_at_unix_ms": ended_at_unix_ms,
        "wall_seconds": wall_elapsed.as_secs_f64(),
        "login_ok": login_ok,
        "ticket_ok": ticket_ok,
        "ws_connected": ws_ok,
        "hello_ok": hello_ok,
        "join_ok": join_ok,
        "join_fail": join_fail,
        "join_latency_ms": duration_stats(&join_latencies),
        "rtt_ms": duration_stats(&rtts),
        "dropped_updates": dropped_updates,
        "messages_received": messages_received,
        "errors": errors,
        "network": {
            "sent_bytes": sent_bytes,
            "received_bytes": received_bytes,
            "sent_bytes_per_second": sent_bytes as f64 / bytes_per_second,
            "received_bytes_per_second": received_bytes as f64 / bytes_per_second,
            "frame_bytes": true,
        },
        "disconnects": disconnects,
        "reconnects": reconnects,
        "ramp_rate": args.ramp_rate,
        "ramp_stages": ramp_stages,
        "ramp_degradation_onset_stage": ramp_degradation_stage,
        "reconnect_interval_seconds": args.reconnect_interval,
        "snapshot_size_bytes": usize_stats(&snapshot_sizes),
        "metrics": metrics_report,
    });
    if let Some(path) = &args.json_output {
        match serde_json::to_vec_pretty(&report)
            .map_err(|error| error.to_string())
            .and_then(|data| std::fs::write(path, data).map_err(|error| error.to_string()))
        {
            Ok(()) => println!("  json_report: {}", path),
            Err(error) => eprintln!("could not write JSON report {}: {error}", path),
        }
    }
    // Failure vs success prominence per spec
    if (args.scenario != "login" && join_fail > 0) || errors > 0 {
        println!();
        println!(
            "  FAILURES ARE PROMINENT: join_fail={} errors={} — a load test that cannot report failure is not a load test",
            join_fail, errors
        );
    }
    if args.scenario != "login" && join_ok == 0 {
        println!();
        println!("  OVERALL: FAIL — no successful joins");
        std::process::exit(1);
    }
    if args.scenario != "login" && rtt_samples == 0 && total_ops_expected > 0 {
        println!();
        println!("  OVERALL: FAIL — no transform round-trips (send path may be broken)");
        std::process::exit(1);
    }
    println!();
    println!(
        "  PARAMS: server_url={} users={} hz={:.2} duration={}s commit={}",
        args.server_url, args.users, args.hz, args.duration, commit
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_position_zero_offsets_preserve_placement() {
        assert_eq!(initial_position("grid", 7, 200, 7.5, 0.0, 0.0), (52.5, 0.0));
        assert_eq!(initial_position("origin", 0, 1, 7.5, 0.0, 0.0), (1.0, 0.0));
    }

    #[test]
    fn test_initial_position_applies_offsets() {
        assert_eq!(
            initial_position("grid", 0, 1, 7.5, 52.5, 48.75),
            (52.5, 48.75)
        );
        assert_eq!(initial_position("origin", 0, 1, 7.5, -1.0, 2.0), (1.0, 0.0));
    }

    #[test]
    fn test_percentile_basic() {
        let v = vec![
            Duration::from_millis(10),
            Duration::from_millis(20),
            Duration::from_millis(30),
            Duration::from_millis(40),
        ];
        assert_eq!(percentile(&v, 0.50), Duration::from_millis(20));
        assert_eq!(percentile(&v, 0.95), Duration::from_millis(40));
        assert_eq!(percentile(&v, 0.99), Duration::from_millis(40));
    }

    #[test]
    fn test_validate_rejects_zero_users() {
        let args = Args {
            server_url: "http://localhost:8080".to_string(),
            ws_url: String::new(),
            users: 0,
            hz: 10.0,
            duration: 10,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "x".to_string(),
            password_file: None,
            instance_id: String::new(),
            commit: String::new(),
            no_auto_create: false,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        };
        assert!(validate(&args).is_err());
    }

    #[test]
    fn test_resolve_password_from_file() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("orbisync-test-pw-{}", uuid::Uuid::now_v7()));
        std::fs::write(&path, "s3cr3t\n").unwrap();
        let args = Args {
            server_url: "http://localhost:8080".to_string(),
            ws_url: String::new(),
            users: 1,
            hz: 10.0,
            duration: 1,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "ignored".to_string(),
            password_file: Some(path.to_string_lossy().to_string()),
            instance_id: String::new(),
            commit: String::new(),
            no_auto_create: false,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        };
        assert_eq!(resolve_password(&args).unwrap(), "s3cr3t");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_resolve_password_prefers_file_over_env() {
        let dir = std::env::temp_dir();
        let path = dir.join(format!("orbisync-test-pw2-{}", uuid::Uuid::now_v7()));
        std::fs::write(&path, "from-file").unwrap();
        let args = Args {
            server_url: "http://localhost:8080".to_string(),
            ws_url: String::new(),
            users: 1,
            hz: 10.0,
            duration: 1,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "from-cli".to_string(),
            password_file: Some(path.to_string_lossy().to_string()),
            instance_id: String::new(),
            commit: String::new(),
            no_auto_create: false,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        };
        assert_eq!(resolve_password(&args).unwrap(), "from-file");
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn test_debug_redacts_password() {
        let args = Args {
            server_url: "http://localhost:8080".to_string(),
            ws_url: String::new(),
            users: 1,
            hz: 10.0,
            duration: 1,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "super-secret".to_string(),
            password_file: None,
            instance_id: String::new(),
            commit: String::new(),
            no_auto_create: false,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        };
        let dbg = format!("{:?}", args);
        assert!(
            !dbg.contains("super-secret"),
            "Debug must not leak password"
        );
        assert!(dbg.contains("[REDACTED]"));
    }

    #[test]
    fn test_ws_url_derivation() {
        assert_eq!(
            ws_url_from("http://localhost:8080", ""),
            "ws://localhost:8080/ws"
        );
        assert_eq!(
            ws_url_from("https://example.com", ""),
            "wss://example.com/ws"
        );
        assert_eq!(
            ws_url_from("http://localhost:8080/", ""),
            "ws://localhost:8080/ws"
        );
        assert_eq!(ws_url_from("http://a", "ws://b/ws"), "ws://b/ws");
    }

    #[test]
    fn test_metrics_parser_keeps_only_implemented_metrics() {
        let parsed = parse_metrics(
            "# HELP process_cpu_seconds_total cpu\nprocess_cpu_seconds_total 4\n\
             instance_command_queue_depth{queue=\"control\"} 3\n\
             outbound_queue_depth 99\ninvalid_metric nope\n",
        );
        assert_eq!(parsed.get("process_cpu_seconds_total"), Some(&4.0));
        assert_eq!(
            parsed.get("instance_command_queue_depth{queue=\"control\"}"),
            Some(&3.0)
        );
        assert_eq!(parsed.get("outbound_queue_depth"), Some(&99.0));
    }

    #[test]
    fn test_metrics_cpu_is_delta_rate_and_missing_is_explicit() {
        let started = Instant::now();
        let mut collector = MetricsCollector::new(None, 5, started);
        collector.unavailable_sample("not configured");
        assert!(collector.report.samples[0].values.is_empty());
        assert!(
            collector.report.samples[0]
                .unavailable_metrics
                .contains(&"process_cpu_seconds_total".to_string())
        );

        let mut collector =
            MetricsCollector::new(Some("http://localhost/metrics".to_string()), 5, started);
        collector.record("process_cpu_seconds_total 2\n", 1_000);
        collector.record("process_cpu_seconds_total 3\n", 3_000);
        assert_eq!(collector.report.samples[1].cpu_rate_per_second, Some(0.5));
    }
}

#[cfg(test)]
mod e2e_mock_tests {
    use super::realtime::{EntityState, JoinAccepted, ServerHello, Snapshot, StateDelta};
    use super::*;
    use axum::extract::ws::{Message as AxumWsMessage, WebSocket, WebSocketUpgrade};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::IntoResponse;
    use axum::routing::{get, post};
    use std::sync::Arc;

    async fn mock_login_handler() -> impl IntoResponse {
        let body = serde_json::json!({
            "access_token": "mock-access-token",
            "token_type": "Bearer",
            "expires_in": 900
        });
        (StatusCode::OK, axum::Json(body))
    }

    async fn mock_ticket_handler(headers: HeaderMap) -> impl IntoResponse {
        let auth = headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if auth.contains("mock-access-token") {
            let body = serde_json::json!({
                "realtime_ticket": "mock-realtime-ticket",
                "expires_in": 60
            });
            (StatusCode::OK, axum::Json(body)).into_response()
        } else {
            (
                StatusCode::UNAUTHORIZED,
                axum::Json(serde_json::json!({"error": "auth"})),
            )
                .into_response()
        }
    }

    async fn mock_world_handler() -> impl IntoResponse {
        let body = serde_json::json!({
            "id": uuid::Uuid::now_v7().to_string(),
            "name": "mock-world",
            "revision": 1,
            "status": "running"
        });
        (StatusCode::CREATED, axum::Json(body))
    }

    async fn mock_instance_handler() -> impl IntoResponse {
        let body = serde_json::json!({
            "id": uuid::Uuid::now_v7().to_string(),
            "world_id": uuid::Uuid::now_v7().to_string(),
            "status": "running",
            "revision": 1
        });
        (StatusCode::CREATED, axum::Json(body))
    }

    async fn mock_ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
        ws.protocols([WEBSOCKET_SUBPROTOCOL])
            .on_upgrade(handle_mock_ws)
    }

    async fn handle_mock_ws(mut socket: WebSocket) {
        // 1. ClientHello -> ServerHello
        let msg = match socket.recv().await {
            Some(Ok(AxumWsMessage::Binary(b))) => b,
            _ => return,
        };
        let env = match Envelope::decode(msg.as_ref()) {
            Ok(e) => e,
            Err(_) => return,
        };
        let is_hello = matches!(env.payload, Some(envelope::Payload::ClientHello(_)));
        if !is_hello {
            return;
        }
        let server_hello = ServerHello {
            negotiated_minor: 0,
            connection_id: uuid::Uuid::now_v7().to_string(),
            heartbeat_interval_ms: 30000,
            server_time_unix_ms: current_unix_ms(),
            negotiated_compression: String::new(),
            enabled_features: Vec::new(),
        };
        let resp = Envelope {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: 0,
            message_id: uuid::Uuid::now_v7().to_string(),
            sequence: 1,
            sent_at_unix_ms: current_unix_ms(),
            instance_id: String::new(),
            payload: Some(envelope::Payload::ServerHello(server_hello)),
        };
        if socket
            .send(AxumWsMessage::Binary(encode_envelope(&resp).into()))
            .await
            .is_err()
        {
            return;
        }
        // 2. JoinInstance -> JoinAccepted + Snapshot
        let msg = match socket.recv().await {
            Some(Ok(AxumWsMessage::Binary(b))) => b,
            _ => return,
        };
        let env = match Envelope::decode(msg.as_ref()) {
            Ok(e) => e,
            Err(_) => return,
        };
        let instance_id = match env.payload {
            Some(envelope::Payload::JoinInstance(ref j)) => j.world_instance_id.clone(),
            _ => return,
        };
        let join_accepted = JoinAccepted {
            presence_id: uuid::Uuid::now_v7().to_string(),
            instance_revision: 1,
            resume_token: "mock-resume-token".to_string(),
            ..Default::default()
        };
        let env_accept = Envelope {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: 0,
            message_id: uuid::Uuid::now_v7().to_string(),
            sequence: 2,
            sent_at_unix_ms: current_unix_ms(),
            instance_id: instance_id.clone(),
            payload: Some(envelope::Payload::JoinAccepted(join_accepted)),
        };
        let snapshot = Snapshot {
            snapshot_id: uuid::Uuid::now_v7().to_string(),
            chunk_index: 0,
            chunk_count: 1,
            instance_revision: 1,
            data: b"{}".to_vec(),
        };
        let env_snap = Envelope {
            protocol_major: PROTOCOL_MAJOR,
            protocol_minor: 0,
            message_id: uuid::Uuid::now_v7().to_string(),
            sequence: 3,
            sent_at_unix_ms: current_unix_ms(),
            instance_id: instance_id.clone(),
            payload: Some(envelope::Payload::Snapshot(snapshot)),
        };
        let _ = socket
            .send(AxumWsMessage::Binary(encode_envelope(&env_accept).into()))
            .await;
        let _ = socket
            .send(AxumWsMessage::Binary(encode_envelope(&env_snap).into()))
            .await;

        // 3. Loop: TransformInput -> StateDelta echo
        let mut revision: u64 = 1;
        while let Some(Ok(AxumWsMessage::Binary(b))) = socket.recv().await {
            let env = match Envelope::decode(b.as_ref()) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if let Some(envelope::Payload::TransformInput(inp)) = env.payload {
                revision += 1;
                let state = EntityState {
                    entity_id: inp.entity_id.clone(),
                    revision,
                    transform: inp.transform,
                    properties: None,
                    ..Default::default()
                };
                let delta = StateDelta {
                    from_revision: revision - 1,
                    to_revision: revision,
                    entities: vec![state],
                };
                let resp = Envelope {
                    protocol_major: PROTOCOL_MAJOR,
                    protocol_minor: 0,
                    message_id: uuid::Uuid::now_v7().to_string(),
                    sequence: 4,
                    sent_at_unix_ms: current_unix_ms(),
                    instance_id: instance_id.clone(),
                    payload: Some(envelope::Payload::StateDelta(delta)),
                };
                let _ = socket
                    .send(AxumWsMessage::Binary(encode_envelope(&resp).into()))
                    .await;
            }
        }
    }

    fn mock_router() -> axum::Router {
        axum::Router::new()
            .route("/v1/auth/login", post(mock_login_handler))
            .route("/v1/realtime/tickets", post(mock_ticket_handler))
            .route("/v1/worlds", post(mock_world_handler))
            .route("/v1/instances", post(mock_instance_handler))
            .route("/ws", get(mock_ws_handler))
            .route("/v1/realtime/ws", get(mock_ws_handler))
    }

    async fn spawn_mock_server() -> (String, tokio::task::JoinHandle<()>) {
        let app = mock_router();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        // small delay for listen
        tokio::time::sleep(Duration::from_millis(50)).await;
        (format!("http://{addr}"), handle)
    }

    #[tokio::test]
    async fn e2e_live_server_has_nonzero_joins_and_messages() {
        let (server_url, _handle) = spawn_mock_server().await;
        let args = Arc::new(Args {
            server_url: server_url.clone(),
            ws_url: String::new(),
            users: 2,
            hz: 5.0,
            duration: 1,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "password123".to_string(),
            password_file: None,
            instance_id: String::new(),
            commit: "test".to_string(),
            no_auto_create: false,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let pw = resolve_password(&args).expect("password");
        let token = http_login(&client, &server_url, &args.login_id, &pw)
            .await
            .expect("login");
        let instance_id = ensure_instance(&client, &server_url, &token, &args.instance_id)
            .await
            .expect("instance");
        let ticks = (args.duration as f64 * args.hz).round() as usize;
        let interval = Duration::from_secs_f64(1.0 / args.hz);
        let r = run_virtual_user(
            0,
            Arc::clone(&args),
            client.clone(),
            instance_id.clone(),
            ticks,
            interval,
            args.login_id.clone(),
            pw.clone(),
            Duration::ZERO,
            Arc::new(Semaphore::new(args.login_concurrency)),
        )
        .await;
        assert!(r.login_ok, "login must succeed against live server");
        assert!(r.ticket_ok, "ticket must succeed");
        assert!(r.ws_connected, "ws must connect");
        assert!(r.hello_ok, "hello must succeed");
        assert!(
            r.join_ok,
            "join must succeed — this proves real network, stub would succeed against nothing"
        );
        assert!(r.join_latency.is_some(), "join latency must be measured");
        assert!(
            !r.transform_rtts.is_empty(),
            "non-zero RTT samples — transform send path is working"
        );
        assert!(r.messages_received > 0, "non-zero messages received");
        // Real check: if we had not done network, these would be zero
        assert!(
            r.errors == 0,
            "no errors expected against mock live server, got {}",
            r.errors
        );
    }

    #[tokio::test]
    async fn e2e_closed_port_reports_failures() {
        // Use a port that is not listening (1 is privileged and closed)
        let closed_url = "http://127.0.0.1:1".to_string();
        let args = Arc::new(Args {
            server_url: closed_url.clone(),
            ws_url: String::new(),
            users: 1,
            hz: 5.0,
            duration: 1,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "password123".to_string(),
            password_file: None,
            instance_id: "00000000-0000-7000-8000-000000000001".to_string(),
            commit: "test".to_string(),
            no_auto_create: true,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        // This run should fail at login (connection refused)
        let ticks = 5;
        let interval = Duration::from_millis(200);
        let pw = resolve_password(&args).unwrap();
        let r = run_virtual_user(
            0,
            Arc::clone(&args),
            client,
            args.instance_id.clone(),
            ticks,
            interval,
            args.login_id.clone(),
            pw,
            Duration::ZERO,
            Arc::new(Semaphore::new(args.login_concurrency)),
        )
        .await;
        assert!(
            !r.login_ok || !r.join_ok,
            "closed port must NOT report success"
        );
        assert!(
            !r.join_ok,
            "join must fail against closed port — stub would have 'succeeded' with fake latencies"
        );
        assert!(r.errors > 0, "failures must be reported");
        assert_eq!(
            r.transform_rtts.len(),
            0,
            "no RTT samples against closed port"
        );
    }

    #[tokio::test]
    async fn mutation_break_transform_send_drops_rtt_to_zero() {
        // This test is the mutation detector: it asserts that the transform send path is exercised.
        // If production code is mutated to skip `TransformInput` sends, this test goes red (rtt_samples == 0).
        let (server_url, _handle) = spawn_mock_server().await;
        let args = Arc::new(Args {
            server_url: server_url.clone(),
            ws_url: String::new(),
            users: 1,
            hz: 10.0,
            duration: 1,
            login_concurrency: 4,
            placement: "grid".to_string(),
            placement_spacing: 7.5,
            placement_offset_x: 0.0,
            placement_offset_z: 0.0,
            login_id: "admin".to_string(),
            password: "password123".to_string(),
            password_file: None,
            instance_id: String::new(),
            commit: "test".to_string(),
            no_auto_create: false,
            scenario: "transform".to_string(),
            ramp_rate: 50.0,
            reconnect_percent: 10.0,
            reconnect_interval: 10,
            slow_consumer_percent: 5.0,
            slow_consumer_delay_ms: 250,
            metrics_url: None,
            metrics_interval: 5,
            json_output: None,
        });
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let pw = resolve_password(&args).unwrap();
        let token = http_login(&client, &server_url, &args.login_id, &pw)
            .await
            .unwrap();
        let instance_id = ensure_instance(&client, &server_url, &token, &args.instance_id)
            .await
            .unwrap();
        let ticks = (args.duration as f64 * args.hz).round() as usize;
        let interval = Duration::from_secs_f64(1.0 / args.hz);
        let r = run_virtual_user(
            0,
            Arc::clone(&args),
            client,
            instance_id,
            ticks,
            interval,
            args.login_id.clone(),
            pw,
            Duration::ZERO,
            Arc::new(Semaphore::new(args.login_concurrency)),
        )
        .await;
        // If transform send were broken (e.g., commenting out the `send TransformInput` block),
        // this would be 0 and the assertion would fail, proving the detector works.
        assert!(
            !r.transform_rtts.is_empty(),
            "MUTATION DETECTOR: rtt_samples must be >0 when transform path is intact; if you break the send, this goes red (rtt_samples=0, messages_received={} errors={})",
            r.messages_received,
            r.errors
        );
    }
}
