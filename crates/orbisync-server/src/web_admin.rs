//! Local operator launcher. Configuration remains the stock TOML + EnvSource contract.
use super::*;
use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Request, State},
    http::{HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use ed25519_dalek::pkcs8::EncodePrivateKey;
use orbisync_config::{Config, MapEnv};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio::sync::Mutex;

const SETTINGS: &[&str] = &[
    "server.bind",
    "database.max_connections",
    "database.acquire_timeout_seconds",
    "database.readiness_timeout_seconds",
    "realtime.max_connections",
    "world.default_capacity",
];
type WebResult<T> = Result<T, (StatusCode, Json<Value>)>;
fn bad(message: impl Into<String>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"message":message.into()})),
    )
}
fn io_error(_: impl std::fmt::Display) -> (StatusCode, Json<Value>) {
    bad(
        "Cannot read or persist private installation files. Check directory ownership and available space.",
    )
}

struct Installation {
    dir: PathBuf,
    config_path: PathBuf,
    existing: bool,
    restart_required: bool,
    engine: Option<std::process::Child>,
    engine_url: Option<String>,
    engine_proof: Option<String>,
    managed_db: Option<local_postgres::LocalPostgres>,
}
struct WebState {
    install: Mutex<Installation>,
    capability: String,
    origin: String,
    client: reqwest::Client,
    operation: std::sync::Mutex<Value>,
    status_cache: std::sync::Mutex<Value>,
    busy: std::sync::atomic::AtomicBool,
    request_log: std::sync::Mutex<std::fs::File>,
}

pub(super) fn run(
    dir: &Path,
    port: u16,
    no_browser: bool,
    existing: Option<&Path>,
) -> Result<(), ServerError> {
    // Explicit --config is the only way to attach a legacy installation. Never
    // infer permission to rewrite a config discovered in the current directory.
    if existing.is_none()
        && !dir.join("orbisync.toml").exists()
        && Config::discover_path(None, &SystemEnv::new())?.is_some()
    {
        return Err(ServerError::BootstrapIo(std::io::Error::other(
            "Existing configuration detected. Use --config PATH web-admin to manage it without initialization.",
        )));
    }
    private_dir(dir).map_err(ServerError::BootstrapIo)?;
    let dir = std::fs::canonicalize(dir).map_err(ServerError::BootstrapIo)?;
    let config_path = existing
        .map(Path::to_path_buf)
        .unwrap_or_else(|| dir.join("orbisync.toml"));
    let runtime = build_runtime(2).map_err(ServerError::Runtime)?;
    runtime.block_on(async {
        let listener = match tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port)).await {
            Ok(listener) => listener,
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).await.map_err(ServerError::Listener)?
            }
            Err(error) => return Err(ServerError::Listener(error)),
        };
        let address = listener.local_addr().map_err(ServerError::Listener)?;
        let origin = format!("http://{address}");
        let capability = random_secret();
        let request_log = private_log(&dir.join("admin.log")).map_err(|_| ServerError::Bootstrap)?;
        let state = Arc::new(WebState {
            install: Mutex::new(Installation {
                dir,
                config_path,
                existing: existing.is_some(),
                restart_required: false,
                engine: None,
                engine_url: None,
                engine_proof: None,
                managed_db: None,
            }),
            capability: capability.clone(),
            origin: origin.clone(),
            operation: std::sync::Mutex::new(json!({"phase":"idle","busy":false,"error_code":null})),
            status_cache: std::sync::Mutex::new(json!({"initialized":false,"existing":existing.is_some(),"engine":"not started","ready":false,"restart_required":false})),
            busy: std::sync::atomic::AtomicBool::new(false),
            request_log: std::sync::Mutex::new(request_log),
            client: reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .map_err(|_| ServerError::Bootstrap)?,
        });
        let app = Router::new()
            .route(
                "/",
                get(|| async { Html(include_str!("../../../apps/admin-web/index.html")) }),
            )
            .route(
                "/admin.js",
                get(|| async {
                    (
                        [("content-type", "text/javascript")],
                        include_str!("../../../apps/admin-web/src/main.js"),
                    )
                }),
            )
            .route("/admin/status", get(status))
            .route("/admin-connection.js", get(|| async { ([("content-type", "text/javascript")], include_str!("../../../apps/admin-web/src/local_connection.js")) }))
            .route("/admin/local-trial", post(local_trial))
            .route("/admin/stop", post(stop))
            .route("/admin/restart", post(restart))
            .route("/admin/diagnostics", get(diagnostics))
            .route("/admin/repair", post(repair))
            .route("/admin/probe", post(probe_connection))
            .route("/admin/check", post(check))
            .route("/admin/initialize", post(initialize))
            .route("/admin/start", post(start))
            .route("/admin/settings", get(settings).post(save_settings))
            .route("/api/{*path}", axum::routing::any(proxy))
            .layer(middleware::from_fn_with_state(state.clone(), protect))
            .with_state(state.clone());
        let url = format!("{origin}/#{capability}");
        // The private link is not emitted into redirected service logs.
        let link = state.install.lock().await.dir.join("launch-url.txt");
        replace(&link, url.as_bytes()).map_err(ServerError::BootstrapIo)?;
        println!(
            "Administration: {origin}/; private access link: {}",
            link.display()
        );
        if std::io::stdout().is_terminal() {
            println!("Open this private link: {url}");
        }
        if !no_browser {
            open_browser(&url);
        }
        axum::serve(listener, app)
            .with_graceful_shutdown(wait_for_signal())
            .await
            .map_err(ServerError::Listener)?;
        let mut install = state.install.lock().await;
        stop_installation(&state, &mut install).await.map_err(|_| ServerError::Bootstrap)?;
        Ok(())
    })
}

async fn protect(State(state): State<Arc<WebState>>, request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().to_string();
    // Route templates exclude IDs, query strings, credentials and request bodies.
    let matched_route = request
        .extensions()
        .get::<axum::extract::MatchedPath>()
        .map_or("unmatched", axum::extract::MatchedPath::as_str)
        .to_owned();
    let route = if matched_route == "/api/{*path}" {
        let segments: Vec<_> = request
            .uri()
            .path()
            .split('/')
            .map(|part| match part {
                ""
                | "api"
                | "v1"
                | "auth"
                | "users"
                | "roles"
                | "worlds"
                | "instances"
                | "login"
                | "logout"
                | "refresh"
                | "me"
                | "change-password"
                | "reset-password"
                | "start"
                | "stop"
                | "enable"
                | "disable"
                | "members"
                | "kick"
                | "realtime"
                | "tickets"
                | "administration-access" => part,
                _ => "{id}",
            })
            .collect();
        segments.join("/")
    } else {
        matched_route
    };
    let request_id = format!("admin_{}", Uuid::now_v7());
    let host = state.origin.strip_prefix("http://").unwrap_or_default();
    let headers = request.headers();
    let valid_host = headers.get("host").and_then(|v| v.to_str().ok()) == Some(host);
    let valid_origin = headers
        .get("origin")
        .is_none_or(|v| v.to_str().ok() == Some(state.origin.as_str()));
    let public_asset = matches!(
        request.uri().path(),
        "/" | "/admin.js" | "/admin-connection.js"
    ) && request.method() == axum::http::Method::GET;
    let authorized = headers
        .get("x-orbisync-local")
        .and_then(|v| v.to_str().ok())
        == Some(state.capability.as_str());
    let mut response = if !valid_host || !valid_origin || (!public_asset && !authorized) {
        StatusCode::FORBIDDEN.into_response()
    } else {
        next.run(request).await
    };
    let engine_request_id = response
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .filter(|id| {
            id.len() <= 80
                && id
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
        });
    // Successful status polling is intentionally omitted. Mutations and failures
    // are durable even though web-admin does not install the engine subscriber.
    if method != "GET" || !response.status().is_success() {
        let entry = json!({"timestamp":time::OffsetDateTime::now_utc().unix_timestamp(),
            "event":"admin.request_completed","request_id":request_id,"engine_request_id":engine_request_id,
            "method":method,"route":route,"status":response.status().as_u16(),"duration_ms":started.elapsed().as_millis() as u64});
        let mut log = state.request_log.lock().unwrap_or_else(|e| e.into_inner());
        if log.metadata().is_ok_and(|m| m.len() > 16 * 1024 * 1024) {
            let _truncated = log.set_len(0);
        }
        if writeln!(log, "{entry}").and_then(|()| log.flush()).is_err() {
            eprintln!("admin.request_log_write_failed");
        }
    }
    if let Ok(value) = HeaderValue::from_str(&request_id) {
        response
            .headers_mut()
            .insert("x-orbisync-admin-request-id", value);
    }
    for (name, value) in [
        ("cache-control", "no-store"),
        ("referrer-policy", "no-referrer"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
        (
            "content-security-policy",
            "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'",
        ),
    ] {
        response.headers_mut().insert(
            axum::http::HeaderName::from_static(name),
            HeaderValue::from_static(value),
        );
    }
    response
}

impl Installation {
    fn initialized(&self) -> bool {
        self.existing || self.config_path.exists()
    }
    fn env(&self) -> WebResult<MapEnv> {
        if self.existing {
            return Ok(MapEnv::from_pairs(std::env::vars()));
        }
        let secrets: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(self.dir.join("secrets.json")).map_err(io_error)?,
        )
        .map_err(io_error)?;
        // Managed values are isolated from ambient configuration overrides. Only
        // extension secret references are inherited by the stock EnvSecretProvider.
        Ok(MapEnv::from_pairs(secrets))
    }
    fn config(&self) -> WebResult<Config> {
        Config::load(Some(&self.config_path), &self.env()?, &[])
            .map(|v| v.config)
            .map_err(|e| bad(format!("{}: {}", e.key(), e.detail())))
    }
}

async fn status(State(state): State<Arc<WebState>>) -> Json<Value> {
    let operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let Ok(mut install) = state.install.try_lock() else {
        let mut cached = state
            .status_cache
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        cached["operation"] = operation;
        return Json(cached);
    };
    let running = install
        .engine
        .as_mut()
        .is_some_and(|engine| engine.try_wait().is_ok_and(|exit| exit.is_none()));
    if !running {
        install.engine_url = None;
        install.engine_proof = None;
    }
    let engine_state = if running {
        "running"
    } else if install.engine.is_some() || operation["phase"] == "stopped" {
        "stopped"
    } else {
        "not started"
    };
    let url = if running && verify_engine_owner(&state, &install).await.is_ok() {
        install.engine_url.clone()
    } else {
        None
    };
    let mut result = json!({"initialized":install.initialized(), "existing":install.existing, "engine":engine_state, "restart_required":install.restart_required});
    result["initialization_pending"] = json!(install.dir.join("initializing").exists());
    // allow-hardcoded-secret: literal is a corpus filename, not a credential.
    let development_corpus = std::fs::read_to_string(install.dir.join("password-denylist.txt"))
        .is_ok_and(|text| {
            text.lines()
                .any(|line| line.starts_with("orbisync-dev-placeholder-"))
        });
    result["development_corpus"] = json!(development_corpus);
    result["local_trial"] = json!(install.dir.join("local-trial.json").is_file());
    result["managed_db"] = install
        .managed_db
        .as_ref()
        .map(|database| json!(database.status()))
        .unwrap_or(Value::Null);
    result["operation"] = operation;
    drop(install);
    let ready = if let Some(url) = url {
        state
            .client
            .get(format!("{url}/health/ready"))
            .timeout(std::time::Duration::from_secs(1))
            .send()
            .await
            .is_ok_and(|r| r.status().is_success())
    } else {
        false
    };
    result["ready"] = json!(ready);
    *state
        .status_cache
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = result.clone();
    Json(result)
}

#[derive(Deserialize)]
struct Setup {
    database_url: String,
    #[serde(default)]
    managed_local: bool,
    #[serde(default)]
    local_development: bool,
    #[serde(default)]
    corpus_path: String,
    #[serde(default)]
    login_id: String,
    #[serde(default)]
    display_name: String,
    #[serde(default = "default_bind")]
    engine_bind: String,
}
fn default_bind() -> String {
    "127.0.0.1:8080".into()
}
async fn connect(url: &str) -> WebResult<sqlx::PgPool> {
    if !(url.starts_with("postgres://") || url.starts_with("postgresql://")) {
        return Err(bad(
            "Database URL must start with postgres:// or postgresql://.",
        ));
    }
    sqlx::postgres::PgPoolOptions::new().max_connections(1).acquire_timeout(std::time::Duration::from_secs(5)).connect(url).await.map_err(|_| bad("Database connection failed. Check host, port, database name, credentials and PostgreSQL access rules."))
}
async fn ensure_empty(pool: &sqlx::PgPool) -> WebResult<()> {
    let occupied:bool = sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname NOT IN ('pg_catalog','information_schema') AND n.nspname NOT LIKE 'pg_toast%' AND c.relkind IN ('r','p','v','m','S'))").fetch_one(pool).await.map_err(|_| bad("Could not inspect database. No initialization performed."))?;
    if occupied {
        return Err(bad(
            "Database already contains tables, views or sequences. Initialization refused. Use the existing installation's config and secrets with --config PATH web-admin.",
        ));
    }
    Ok(())
}
async fn check(
    State(state): State<Arc<WebState>>,
    Json(body): Json<Setup>,
) -> WebResult<Json<Value>> {
    if state.install.lock().await.initialized() {
        return Err(bad("Setup is closed for this installation."));
    }
    let pool = connect(&body.database_url).await?;
    let result = ensure_empty(&pool).await;
    pool.close().await;
    result?;
    Ok(Json(
        json!({"message":"Connection successful. Database is empty and eligible for initialization."}),
    ))
}

async fn initialize(
    State(state): State<Arc<WebState>>,
    Json(body): Json<Setup>,
) -> WebResult<Json<Value>> {
    let mut install = state.install.lock().await;
    if install.initialized() || install.dir.join("initializing").exists() {
        return Err(bad(
            "Setup is already closed or a previous initialization needs recovery. Existing files and database are preserved.",
        ));
    }
    let login = LoginId::new(body.login_id).map_err(|_| bad("Administrator login is invalid."))?;
    if body.display_name.trim().is_empty()
        || body.display_name.chars().count() > 128
        || body.display_name.chars().any(char::is_control)
    {
        return Err(bad("Display name must contain 1–128 printable characters."));
    }
    let mut config = Config::default();
    config
        .apply("server.bind", &body.engine_bind)
        .map_err(|e| bad(e.detail()))?;
    config
        .validate()
        .map_err(|e| bad(format!("{}: {}", e.key(), e.detail())))?;
    loopback_bind(&config.server.bind)?;
    // Same explicitly development-only corpus as scripts/gen-dev-password-denylist.sh.
    let entries = if body.local_development {
        (1..=10_000)
            .map(|n| format!("orbisync-dev-placeholder-{n:05}"))
            .collect()
    } else {
        if body.corpus_path.is_empty() {
            return Err(bad(
                "Production setup requires an operator-provided password corpus path (exactly 10,000 distinct lines).",
            ));
        }
        read_password_corpus(Path::new(&body.corpus_path))
            .map_err(|_| bad("Cannot read operator password corpus."))?
    };
    if !body.local_development
        && entries
            .iter()
            .any(|s| s.starts_with("orbisync-dev-placeholder-"))
    {
        return Err(bad(
            "Development placeholder corpus cannot be selected for production.",
        ));
    }
    let policy = PasswordPolicy::production(entries.clone())
        .map_err(|_| bad("Password corpus must contain exactly 10,000 distinct valid entries."))?
        .with_min_length(config.auth.password_min_length);
    let passwords = PasswordService::new_with_argon2_and_concurrency(
        policy,
        config.auth.argon2_memory_cost_kib,
        config.auth.argon2_iterations,
        config.auth.argon2_parallelism,
        config.auth.password_hash_concurrency as usize,
    )
    .map_err(|_| bad("Password policy could not be initialized."))?;
    let pool = connect(&body.database_url).await?;
    if let Err(error) = ensure_empty(&pool).await {
        pool.close().await;
        return Err(error);
    }
    // Close setup before any mutation. Failure never silently retries bootstrap.
    write_new(
        &install.dir.join("initializing"),
        b"Initialization begun. Preserve this directory for recovery.\n",
    )
    .map_err(io_error)?;
    // allow-hardcoded-secret: literal is a corpus filename, not a credential.
    let corpus = install.dir.join("password-denylist.txt");
    write_new(&corpus, entries.join("\n").as_bytes()).map_err(io_error)?;
    let mut secrets = BTreeMap::new();
    secrets.insert(config.database.url_env.clone(), body.database_url);
    for key in config.required_secret_env_vars().into_iter().skip(1) {
        secrets.insert(key.to_string(), random_secret());
    }
    let signing = ed25519_dalek::SigningKey::from_bytes(&rand::random());
    secrets.insert(
        config.auth.token_signing_key_env.clone(),
        signing
            .to_pkcs8_pem(Default::default())
            .map_err(io_error)?
            .to_string(),
    );
    secrets.insert(
        PASSWORD_DENYLIST_ENV.to_string(),
        corpus.to_string_lossy().into_owned(),
    );
    write_new(
        &install.dir.join("secrets.json"),
        &serde_json::to_vec(&secrets).map_err(io_error)?,
    )
    .map_err(io_error)?;
    let config_text = format!(
        "# Generated by web-admin; standard OrbiSync configuration.\n# local_development = {}\n[server]\nbind = {}\n",
        body.local_development,
        toml::Value::String(body.engine_bind)
    );
    write_new(&install.config_path, config_text.as_bytes()).map_err(io_error)?;
    write_new(
        &install.dir.join("input-rules.json"),
        b"{\"version\":1,\"rules\":[]}",
    )
    .map_err(io_error)?;
    if body.managed_local {
        let database = install
            .managed_db
            .as_ref()
            .ok_or_else(|| local_error("database_identity_mismatch"))?;
        if database.application_database_url()
            != secrets
                .get(&config.database.url_env)
                .cloned()
                .unwrap_or_default()
        {
            return Err(local_error("database_identity_mismatch"));
        }
        let migration_pool = connect(&database.migration_database_url()).await?;
        let migrated = run_migrations(&migration_pool).await;
        migration_pool.close().await;
        migrated.map_err(|_| local_error("migration_failed"))?;
        database
            .grant_runtime_access()
            .await
            .map_err(database_error)?;
    } else {
        if let Some(database) = install.managed_db.as_mut() {
            database.stop().await.map_err(database_error)?;
        }
        install.managed_db.take();
        let marker = install.dir.join("local-trial.json");
        if marker.is_file() {
            std::fs::remove_file(marker).map_err(io_error)?;
        }
        run_migrations(&pool).await.map_err(|_| bad("Migrations failed. Setup is closed; preserve the installation directory and inspect PostgreSQL permissions before recovery."))?;
    }
    let env = MapEnv::from_pairs(secrets);
    let query = PgIdentityQueryStore::new(pool.clone(), &config, &env)
        .map_err(|_| bad("Identity query could not be initialized."))?;
    ensure_bootstrap_allowed(&query)
        .await
        .map_err(|_| bad("Administrator bootstrap refused: database already has users."))?;
    let service = IdentityAdministrationService::new(
        Arc::new(IdentityAdministrationStore::new(pool.clone())),
        Arc::new(SystemClock::new()),
        passwords,
    );
    let request_id = orbisync_application::RequestId::new(format!("req_{}", UserId::generate()))
        .map_err(|_| bad("Cannot create bootstrap request."))?;
    // allow-hardcoded-secret: literal is a redacted failure message; password comes from the service.
    let (_, password) = service.bootstrap_administrator(login,body.display_name,request_id).await.map_err(|_| bad("Administrator bootstrap failed. Setup remains closed; use documented CLI recovery."))?;
    // Crash/reload recovery is private on disk, never a repeatable web endpoint.
    write_new(
        // allow-hardcoded-secret: literal is the private recovery filename, not its contents.
        &install.dir.join("initial-admin-password.txt"),
        password.expose_secret().as_bytes(),
    )
    .map_err(io_error)?;
    std::fs::remove_file(install.dir.join("initializing")).map_err(io_error)?;
    pool.close().await;
    Ok(Json(
        // allow-hardcoded-secret: message is UI guidance; the password is freshly generated.
        json!({"temporary_password":password.expose_secret(),"message":"Initialized. Save the one-time password, start the engine, then sign in and change it."}),
    ))
}

fn loopback_bind(bind: &str) -> WebResult<std::net::SocketAddr> {
    let addr: std::net::SocketAddr = bind
        .parse()
        .map_err(|_| bad("Engine address must be an IP address and port."))?;
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err(bad(
            "Web administration requires a loopback engine address with a nonzero port. Use the ordinary serve CLI for public deployment.",
        ));
    }
    Ok(addr)
}
async fn start(State(state): State<Arc<WebState>>) -> WebResult<Json<Value>> {
    let _operation = LocalOperation::begin(&state, "db_starting")?;
    let result = start_installation(&state, &mut *state.install.lock().await).await;
    complete_operation(&state, &result, "ready");
    result.map(|()| Json(json!({"message":"Engine ready.","ready":true})))
}

async fn require_admin(state: &WebState, headers: &HeaderMap) -> WebResult<()> {
    let url = verified_engine_url(state, &mut *state.install.lock().await).await?;
    let token = headers.get("authorization").ok_or_else(|| {
        (
            StatusCode::UNAUTHORIZED,
            Json(json!({"message":"Administrator login required."})),
        )
    })?;
    // allow-hardcoded-secret: formatted value is a loopback endpoint; authorization comes from headers.
    let response = state.client.get(format!("{url}/v1/auth/administration-access")).header("authorization",token).send().await.map_err(|_| bad("Engine unavailable. Configuration cannot be changed without administrator verification."))?;
    if !response.status().is_success() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(
                json!({"message":"An active administrator session with admin.roles.assign is required."}),
            ),
        ));
    }
    Ok(())
}
async fn settings(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> WebResult<Json<Value>> {
    require_admin(&state, &headers).await?;
    let install = state.install.lock().await;
    let config = install.config()?;
    let values = json!({"server.bind":config.server.bind,"database.max_connections":config.database.max_connections.to_string(),"database.acquire_timeout_seconds":config.database.acquire_timeout_seconds.to_string(),"database.readiness_timeout_seconds":config.database.readiness_timeout_seconds.to_string(),"realtime.max_connections":config.realtime.max_connections.to_string(),"world.default_capacity":config.world.default_capacity.to_string()});
    let manifest = if install.existing {
        json!(null)
    } else {
        serde_json::from_slice::<Value>(
            &std::fs::read(install.dir.join("input-rules.json")).map_err(io_error)?,
        )
        .map_err(io_error)?
    };
    let guest = json!({
        "enabled":config.auth.methods.iter().any(|method| method == "guest"),
        "allowed_worlds":config.auth.guest.allowed_worlds,
        "role_names":config.auth.guest.role_names,
    });
    Ok(Json(
        json!({"values":values,"manifest":manifest,"guest":guest,"read_only":install.existing,"restart_required":install.restart_required}),
    ))
}
#[derive(Deserialize)]
struct SettingsEdit {
    values: BTreeMap<String, String>,
    #[serde(default)]
    database_url: String,
    manifest: Value,
    #[serde(default)]
    guest: Option<GuestSettingsEdit>,
}
async fn save_settings(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
    Json(body): Json<SettingsEdit>,
) -> WebResult<Json<Value>> {
    require_admin(&state, &headers).await?;
    let mut install = state.install.lock().await;
    if install.existing {
        return Err(bad(
            "Existing installations are read-only here. Edit their original configuration using the ordinary operator workflow.",
        ));
    }
    if install.dir.join("local-trial.json").is_file() && !body.database_url.is_empty() {
        return Err(local_error("managed_database_fixed"));
    }
    let mut config = install.config()?;
    let mut table: toml::Table = std::fs::read_to_string(&install.config_path)
        .map_err(io_error)?
        .parse()
        .map_err(io_error)?;
    for (key, value) in &body.values {
        if !SETTINGS.contains(&key.as_str()) {
            return Err(bad("Unsupported setting."));
        }
        config
            .apply(key, value)
            .map_err(|e| bad(format!("{}: {}", e.key(), e.detail())))?;
        let (section, field) = key
            .split_once('.')
            .ok_or_else(|| bad("Invalid setting key."))?;
        let section = table
            .entry(section)
            .or_insert(toml::Value::Table(toml::Table::new()))
            .as_table_mut()
            .ok_or_else(|| bad("Invalid configuration section."))?;
        let val = if key == "server.bind" {
            toml::Value::String(value.clone())
        } else {
            toml::Value::Integer(value.parse().map_err(|_| bad("Expected an integer."))?)
        };
        section.insert(field.into(), val);
    }
    if let Some(guest) = &body.guest {
        edit_guest_config(&mut config, &mut table, guest)?;
    }
    config
        .validate()
        .map_err(|e| bad(format!("{}: {}", e.key(), e.detail())))?;
    loopback_bind(&config.server.bind)?;
    let manifest:orbisync_server::external_input::InputRuleManifest=serde_json::from_value(body.manifest.clone()).map_err(|_| bad("Input manifest must match version 1 schema (world_id, rule, component_key, endpoint, signing_secret_ref)."))?;
    let transport = orbisync_server::external_input::ExternalInputTransport::new(
        Arc::new(
            ReqwestPreCommitClient::try_new(
                Arc::new(SystemDnsResolver),
                config.extensions.allow_loopback_endpoints,
            )
            .map_err(|_| bad("Cannot initialize input transport."))?,
        ),
        Arc::new(EnvSecretProvider),
        PreCommitValidationPolicy::from_config(config.extensions.clone())
            .map_err(|_| bad("Invalid input policy."))?,
    );
    manifest.register(Arc::new(transport)).await.map_err(bad)?;
    if body.guest.is_some() && config.auth.methods.iter().any(|method| method == "guest") {
        validate_guest_setup(&state, &mut install, &headers, &config, &body.database_url).await?;
    }
    if !body.database_url.is_empty() {
        let pool = connect(&body.database_url).await?;
        // Database replacement must be an existing engine DB, never initialize here.
        let migrated: bool = sqlx::query_scalar("SELECT to_regclass('public.users') IS NOT NULL")
            .fetch_one(&pool)
            .await
            .map_err(|_| bad("Could not inspect replacement database."))?;
        pool.close().await;
        if !migrated {
            return Err(bad(
                "Replacement database must already contain an initialized OrbiSync installation.",
            ));
        }
        let mut secrets: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(install.dir.join("secrets.json")).map_err(io_error)?,
        )
        .map_err(io_error)?;
        secrets.insert(config.database.url_env.clone(), body.database_url);
        replace(
            &install.dir.join("secrets.json"),
            &serde_json::to_vec(&secrets).map_err(io_error)?,
        )
        .map_err(io_error)?;
    }
    replace(
        &install.config_path,
        toml::to_string_pretty(&table).map_err(io_error)?.as_bytes(),
    )
    .map_err(io_error)?;
    replace(
        &install.dir.join("input-rules.json"),
        &serde_json::to_vec_pretty(&body.manifest).map_err(io_error)?,
    )
    .map_err(io_error)?;
    install.restart_required = true;
    Ok(Json(
        json!({"message":"Saved. Restart the engine to apply changes. Guest settings apply to new guest sign-ins; leave and Join again in the app.","restart_required":true}),
    ))
}
async fn proxy(State(state): State<Arc<WebState>>, request: Request) -> Response {
    let path = request
        .uri()
        .path()
        .strip_prefix("/api")
        .unwrap_or_default();
    if !(path.starts_with("/v1/") || matches!(path, "/health/ready" | "/health/live" | "/version"))
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let url = match verified_engine_url(&state, &mut *state.install.lock().await).await {
        Ok(url) => url,
        Err(error) => return error.into_response(),
    };
    let target = format!(
        "{url}{path}{}",
        request
            .uri()
            .query()
            .map(|q| format!("?{q}"))
            .unwrap_or_default()
    );
    let mut outgoing = state.client.request(request.method().clone(), target);
    for name in [
        "authorization",
        "content-type",
        "idempotency-key",
        "if-match",
        "accept",
    ] {
        if let Some(value) = request.headers().get(name) {
            outgoing = outgoing.header(name, value);
        }
    }
    let bytes = match to_bytes(request.into_body(), 1024 * 1024).await {
        Ok(v) => v,
        Err(_) => return StatusCode::PAYLOAD_TOO_LARGE.into_response(),
    };
    let result = match outgoing.body(bytes).send().await {
        Ok(v) => v,
        Err(_) => return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(
                json!({"message":"Engine is unavailable. Check readiness and server diagnostic."}),
            ),
        )
            .into_response(),
    };
    let status = result.status();
    let headers = result.headers().clone();
    let bytes = match result.bytes().await {
        Ok(v) => v,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let mut response = (status, Body::from(bytes)).into_response();
    for name in ["content-type", "etag", "retry-after", "x-request-id"] {
        if let Some(value) = headers.get(name) {
            response
                .headers_mut()
                .insert(axum::http::HeaderName::from_static(name), value.clone());
        }
    }
    response
}
fn random_secret() -> String {
    STANDARD.encode(rand::random::<[u8; 32]>())
}
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}
fn replace(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let temp = path.with_extension(format!("{}.tmp", Uuid::now_v7()));
    write_new(&temp, bytes)?;
    std::fs::rename(&temp, path)
}
pub(super) fn private_dir(path: &Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let system_directory = PathBuf::from(
        std::env::var_os("SystemRoot")
            .ok_or_else(|| std::io::Error::other("Windows system directory is unavailable."))?,
    )
    .join("System32");
    if !path.exists() {
        #[allow(unused_mut)] // Mutated by the Unix permissions extension.
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt as _;
            builder.mode(0o700);
        }
        builder.create(path)?;
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            let user = std::process::Command::new(system_directory.join("whoami.exe"))
                .args(["/user", "/fo", "csv", "/nh"])
                .creation_flags(0x08000000)
                .output()?;
            let output = String::from_utf8_lossy(&user.stdout);
            let sid = output
                .trim()
                .split(',')
                .next_back()
                .unwrap_or_default()
                .trim_matches('"');
            if !user.status.success() || !sid.starts_with("S-1-") {
                return Err(std::io::Error::other("Cannot determine directory owner."));
            }
            // Elevated Windows tokens can create an Administrators-owned
            // directory. Assign only this newly created directory to the
            // current user before enforcing the private ACL below.
            let owner = std::process::Command::new(system_directory.join("icacls.exe"))
                .arg(path)
                .args(["/setowner", &format!("*{sid}")])
                .creation_flags(0x08000000)
                .output()?;
            if !owner.status.success() {
                return Err(std::io::Error::other("Cannot set private directory owner."));
            }
            let result = std::process::Command::new(system_directory.join("icacls.exe"))
                .arg(path)
                .args(["/inheritance:r", "/grant:r", &format!("*{sid}:(OI)(CI)F")])
                .creation_flags(0x08000000)
                .output()?;
            if !result.status.success() {
                return Err(std::io::Error::other(
                    "Cannot protect installation directory.",
                ));
            }
        }
    }
    if std::fs::symlink_metadata(path)?.file_type().is_symlink() || !path.is_dir() {
        return Err(std::io::Error::other(
            "Installation directory must be a real private directory.",
        ));
    }
    #[cfg(windows)]
    {
        use std::os::windows::{fs::MetadataExt as _, process::CommandExt as _};
        if std::fs::symlink_metadata(path)?.file_attributes() & 0x400 != 0 {
            return Err(std::io::Error::other(
                "Installation directory must not be a reparse point.",
            ));
        }
        // Pass the path as an environment value, never interpolate shell code.
        let check = std::process::Command::new(system_directory.join("WindowsPowerShell/v1.0/powershell.exe"))
            // Avoid importing PowerShell modules: a launcher started by pwsh
            // may inherit its incompatible PSModulePath in Windows PowerShell.
            .args(["-NoProfile", "-NonInteractive", "-Command", "$ErrorActionPreference='Stop'; $a=[System.IO.Directory]::GetAccessControl($env:ORBISYNC_PRIVATE_DIR); $sid=[System.Security.Principal.WindowsIdentity]::GetCurrent().User.Value; if ($a.GetOwner([System.Security.Principal.SecurityIdentifier]).Value -ne $sid) { exit 1 }; foreach($r in $a.GetAccessRules($true,$true,[System.Security.Principal.SecurityIdentifier])) { if($r.AccessControlType -eq [System.Security.AccessControl.AccessControlType]::Allow -and $r.IdentityReference.Value -notin @($sid,'S-1-5-18','S-1-5-32-544')) { exit 1 } }; exit 0"])
            .env_remove("PSModulePath")
            .env("ORBISYNC_PRIVATE_DIR", path).creation_flags(0x08000000).output()?;
        if !check.status.success() {
            return Err(std::io::Error::other(
                "Installation directory must be owned by the current user and accessible only to that user, SYSTEM and Administrators.",
            ));
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        if std::fs::metadata(path)?.permissions().mode() & 0o077 != 0 {
            return Err(std::io::Error::other(
                "Installation directory must have mode 0700.",
            ));
        }
    }
    Ok(())
}
fn open_browser(url: &str) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let _browser = std::process::Command::new("rundll32")
            .args(["url.dll,FileProtocolHandler", url])
            .creation_flags(0x08000000)
            .spawn();
    }
    #[cfg(target_os = "macos")]
    {
        let _browser = std::process::Command::new("open").arg(url).spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        let _browser = std::process::Command::new("xdg-open")
            .arg(url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
}

include!("web_admin_local.rs");
include!("web_admin_guest.rs");
