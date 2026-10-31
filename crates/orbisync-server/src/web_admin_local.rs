// allow-hardcoded-secret: this literal is a private installation filename, never credential data.
const TRIAL_CREDENTIALS_FILE: &str = "local-credentials.json";
// allow-hardcoded-secret: this literal is a private installation filename, never credential data.
const TRIAL_METADATA_FILE: &str = "local-trial.json";
// allow-hardcoded-secret: this literal is a private installation filename, never credential data.
const INITIAL_ADMIN_PASSWORD_FILE: &str = "initial-admin-password.txt";
// allow-hardcoded-secret: this literal is a private installation filename, never credential data.
const LOCAL_CORPUS_FILE: &str = "password-denylist.txt";

// Local trial orchestration, included in web_admin. Secrets stay in its private installation.
use std::sync::atomic::Ordering as AtomicOrdering;

fn local_error(code: &str) -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(
            json!({"code":code,"message":"The local operation could not complete. Use the diagnostic actions to retry."}),
        ),
    )
}

fn database_error(error: local_postgres::LocalPostgresError) -> (StatusCode, Json<Value>) {
    local_error(error.code())
}

fn engine_alive(install: &mut Installation) -> bool {
    let alive = install
        .engine
        .as_mut()
        .is_some_and(|engine| engine.try_wait().is_ok_and(|status| status.is_none()));
    if !alive {
        install.engine_url = None;
        install.engine_proof = None;
    }
    alive
}
async fn verify_engine_owner(state: &WebState, install: &Installation) -> WebResult<()> {
    use hmac::Mac as _;
    let url = install
        .engine_url
        .as_deref()
        .ok_or_else(|| local_error("engine_unavailable"))?;
    let secret = install
        .engine_proof
        .as_deref()
        .ok_or_else(|| local_error("engine_identity_mismatch"))?;
    let challenge = random_secret();
    let response = state
        .client
        .get(format!("{url}/health/local-owner"))
        .header("x-orbisync-challenge", &challenge)
        .timeout(std::time::Duration::from_secs(1))
        .send()
        .await
        .map_err(|_| local_error("engine_identity_mismatch"))?;
    let encoded = response
        .headers()
        .get("x-orbisync-launcher-proof")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| local_error("engine_identity_mismatch"))?;
    let proof = STANDARD
        .decode(encoded)
        .map_err(|_| local_error("engine_identity_mismatch"))?;
    let mut mac = hmac::Hmac::<sha2::Sha256>::new_from_slice(secret.as_bytes())
        .map_err(|_| local_error("engine_identity_mismatch"))?;
    mac.update(challenge.as_bytes());
    mac.verify_slice(&proof)
        .map_err(|_| local_error("engine_identity_mismatch"))
}
async fn verified_engine_url(state: &WebState, install: &mut Installation) -> WebResult<String> {
    if !engine_alive(install) {
        return Err(local_error("engine_unavailable"));
    }
    verify_engine_owner(state, install).await?;
    install
        .engine_url
        .clone()
        .ok_or_else(|| local_error("engine_unavailable"))
}

struct LocalOperation<'a>(&'a WebState);
impl<'a> LocalOperation<'a> {
    fn begin(state: &'a WebState, phase: &str) -> WebResult<Self> {
        if state
            .busy
            .compare_exchange(false, true, AtomicOrdering::AcqRel, AtomicOrdering::Acquire)
            .is_err()
        {
            return Err((
                StatusCode::CONFLICT,
                Json(
                    json!({"code":"operation_in_progress","message":"A local operation is already running."}),
                ),
            ));
        }
        set_phase(state, phase);
        Ok(Self(state))
    }
}
impl Drop for LocalOperation<'_> {
    fn drop(&mut self) {
        let mut operation = self
            .0
            .operation
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if !matches!(
            operation.get("phase").and_then(Value::as_str),
            Some("ready" | "stopped" | "error")
        ) {
            operation["phase"] = json!("error");
            operation["error_code"] = json!("operation_interrupted");
        }
        operation["busy"] = json!(false);
        self.0.busy.store(false, AtomicOrdering::Release);
    }
}

fn set_phase(state: &WebState, phase: &str) {
    *state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner()) =
        json!({"phase":phase,"busy":true,"error_code":null});
}
fn complete_operation<T>(state: &WebState, result: &WebResult<T>, success: &str) {
    let mut operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    operation["phase"] = json!(if result.is_ok() { success } else { "error" });
    operation["error_code"] = result
        .as_ref()
        .err()
        .and_then(|(_, body)| body.0.get("code"))
        .cloned()
        .unwrap_or(if result.is_err() {
            json!("operation_failed")
        } else {
            Value::Null
        });
}

fn available_engine_bind() -> WebResult<String> {
    let listener =
        std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).map_err(io_error)?;
    Ok(listener.local_addr().map_err(io_error)?.to_string())
}

async fn start_installation(state: &WebState, install: &mut Installation) -> WebResult<()> {
    if !install.initialized() || install.dir.join("initializing").exists() {
        return Err(local_error("initialization_required"));
    }
    if install.dir.join(TRIAL_METADATA_FILE).is_file() {
        if !local_postgres::LocalPostgres::managed(&install.dir) {
            return Err(local_error("recovery_required"));
        }
        set_phase(state, "db_starting");
        if install.managed_db.is_none() {
            install.managed_db =
                Some(local_postgres::LocalPostgres::open(&install.dir).map_err(database_error)?);
        }
        let database = install
            .managed_db
            .as_mut()
            .ok_or_else(|| local_error("database_unavailable"))?;
        let url = database.start().await.map_err(database_error)?;
        let migration_url = database.migration_database_url();
        let config = install.config()?;
        let mut secrets: BTreeMap<String, String> = serde_json::from_slice(
            &std::fs::read(install.dir.join("secrets.json")).map_err(io_error)?,
        )
        .map_err(io_error)?;
        if secrets.get(&config.database.url_env) != Some(&url) {
            return Err(local_error("database_identity_mismatch"));
        }
        secrets.clear();
        // This verified, launcher-owned trial is upgradeable. Ordinary
        // operator/external databases keep their explicit migration workflow.
        let migration_pool = create_pool(&config.database, &migration_url)
            .map_err(|_| local_error("database_migration_failed"))?;
        let migrated = run_migrations(&migration_pool).await;
        migration_pool.close().await;
        migrated.map_err(|_| local_error("database_migration_failed"))?;
    }
    if let Some(engine) = install.engine.as_mut()
        && engine.try_wait().map_err(io_error)?.is_none()
    {
        return wait_engine_ready(state, install).await;
    }
    install.engine.take();
    set_phase(state, "engine_starting");
    let config = install.config()?;
    let address = loopback_bind(&config.server.bind)?;
    if address.to_string() == state.origin.strip_prefix("http://").unwrap_or_default() {
        return Err(local_error("engine_port_in_use"));
    }
    let env = install.env()?;
    config
        .verify_secrets(&env)
        .map_err(|_| local_error("configuration_invalid"))?;
    let corpus = env
        .get(PASSWORD_DENYLIST_ENV)
        .ok_or_else(|| local_error("corpus_missing"))?;
    PasswordPolicy::production(
        read_password_corpus(Path::new(&corpus)).map_err(|_| local_error("corpus_invalid"))?,
    )
    .map_err(|_| local_error("corpus_invalid"))?;
    create_token_service(&env, &config).map_err(|_| local_error("signing_key_invalid"))?;
    let probe =
        std::net::TcpListener::bind(address).map_err(|_| local_error("engine_port_in_use"))?;
    drop(probe);
    let executable = std::env::current_exe().map_err(io_error)?;
    let input_rules = if install.existing {
        std::env::var_os("ORBISYNC_INPUT_RULES_FILE").map(PathBuf::from)
    } else {
        Some(install.dir.join("input-rules.json"))
    };
    let mut command = std::process::Command::new(executable);
    command.arg("--config").arg(&install.config_path);
    if let Some(path) = input_rules.as_ref() {
        command.arg("--input-rules").arg(path);
    }
    command.arg("managed-serve");
    // Preserve OS/runtime variables, but only accept engine settings from the
    // managed EnvSource. Explicit external-rule secret references are copied below.
    command.env_clear().envs(
        std::env::vars().filter(|(key, _)| !key.starts_with("ORBISYNC_") && key != "DATABASE_URL"),
    );
    for name in env.names_with_prefix("") {
        if let Some(value) = env.get(&name) {
            command.env(name, value);
        }
    }
    let proof = random_secret();
    command.env("ORBISYNC_LOCAL_ENGINE_PROOF", &proof);
    if let Some(path) = input_rules.as_ref()
        && let Ok(value) = std::fs::read(path).and_then(|bytes| {
            serde_json::from_slice::<Value>(&bytes).map_err(std::io::Error::other)
        })
    {
        for reference in value
            .get("rules")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|rule| rule.get("signing_secret_ref").and_then(Value::as_str))
        {
            if let Ok(value) = std::env::var(reference) {
                command.env(reference, value);
            }
        }
    }
    let log = private_log(&install.dir.join("engine.log"))?;
    command
        .stdin(std::process::Stdio::piped())
        .stdout(log.try_clone().map_err(io_error)?)
        .stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x08000000);
    }
    install.engine = Some(
        command
            .spawn()
            .map_err(|_| local_error("engine_start_failed"))?,
    );
    install.engine_url = Some(format!("http://{address}"));
    install.engine_proof = Some(proof);
    install.restart_required = false;
    wait_engine_ready(state, install).await
}

fn private_log(path: &Path) -> WebResult<std::fs::File> {
    if path.exists() {
        let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
        if metadata.file_type().is_symlink() || !metadata.is_file() {
            return Err(local_error("private_storage"));
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt as _;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(local_error("private_storage"));
            }
        }
        if metadata.len() > 16 * 1024 * 1024 {
            std::fs::remove_file(path).map_err(io_error)?;
        }
    }
    let mut options = std::fs::OpenOptions::new();
    options.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    options.open(path).map_err(io_error)
}

async fn wait_engine_ready(state: &WebState, install: &mut Installation) -> WebResult<()> {
    let url = install
        .engine_url
        .as_ref()
        .ok_or_else(|| local_error("engine_start_failed"))?;
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        if install
            .engine
            .as_mut()
            .is_none_or(|engine| !engine.try_wait().is_ok_and(|status| status.is_none()))
        {
            return Err(local_error("engine_start_failed"));
        }
        if verify_engine_owner(state, install).await.is_ok()
            && state
                .client
                .get(format!("{url}/health/ready"))
                .timeout(std::time::Duration::from_secs(1))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(local_error("engine_readiness_timeout"));
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn stop_installation(state: &WebState, install: &mut Installation) -> WebResult<()> {
    set_phase(state, "stopping");
    if let Some(engine) = install.engine.as_mut() {
        drop(engine.stdin.take());
        let config = install.config().ok();
        let seconds = config.map_or(50, |config| {
            config.server.shutdown_drain_timeout_seconds
                + config.server.shutdown_force_timeout_seconds
                + 10
        });
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(seconds);
        loop {
            let engine = install
                .engine
                .as_mut()
                .ok_or_else(|| local_error("engine_stop_failed"))?;
            if engine.try_wait().map_err(io_error)?.is_some() {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                engine.kill().map_err(io_error)?;
                engine.wait().map_err(io_error)?;
                install.engine.take();
                install.engine_url = None;
                return Err(local_error("engine_stop_timeout"));
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    }
    install.engine.take();
    install.engine_url = None;
    install.engine_proof = None;
    if let Some(database) = install.managed_db.as_mut() {
        database.stop().await.map_err(database_error)?;
    }
    Ok(())
}

async fn stop(State(state): State<Arc<WebState>>) -> WebResult<Json<Value>> {
    let _operation = LocalOperation::begin(&state, "stopping")?;
    let result = stop_installation(&state, &mut *state.install.lock().await).await;
    complete_operation(&state, &result, "stopped");
    result.map(|()| Json(json!({"ok":true})))
}
async fn restart(State(state): State<Arc<WebState>>) -> WebResult<Json<Value>> {
    let _operation = LocalOperation::begin(&state, "stopping")?;
    let result = async {
        let mut install = state.install.lock().await;
        stop_installation(&state, &mut install).await?;
        start_installation(&state, &mut install).await
    }
    .await;
    complete_operation(&state, &result, "ready");
    result.map(|()| Json(json!({"ok":true,"ready":true})))
}

#[derive(serde::Serialize, Deserialize, Default)]
struct TrialMetadata {
    #[serde(default)]
    complete: bool,
    #[serde(default)]
    world_id: Option<String>,
    #[serde(default)]
    instance_id: Option<String>,
}
#[derive(serde::Serialize, Deserialize)]
struct TrialCredentials {
    login_id: String,
    password: String,
    #[serde(default)]
    pending_password: Option<String>,
    #[serde(default)]
    needs_rotation: bool,
}
fn read_private_json<T: serde::de::DeserializeOwned>(path: &Path) -> WebResult<T> {
    let metadata = std::fs::symlink_metadata(path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() || metadata.len() > 256 * 1024 {
        return Err(local_error("private_storage"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        if metadata.file_attributes() & 0x400 != 0 {
            return Err(local_error("private_storage"));
        }
    }
    serde_json::from_slice(&std::fs::read(path).map_err(io_error)?).map_err(io_error)
}
fn save_private_json(path: &Path, value: &impl serde::Serialize) -> WebResult<()> {
    replace(path, &serde_json::to_vec(value).map_err(io_error)?).map_err(io_error)
}

async fn local_trial(State(state): State<Arc<WebState>>) -> WebResult<Json<Value>> {
    let _operation = LocalOperation::begin(&state, "db_preparing")?;
    let result = create_or_resume_trial(&state).await;
    complete_operation(&state, &result, "ready");
    result.map(Json)
}

async fn create_or_resume_trial(state: &Arc<WebState>) -> WebResult<Value> {
    let (directory, database_url, initializing, initialized, first) = {
        let mut install = state.install.lock().await;
        let marker = install.dir.join(TRIAL_METADATA_FILE);
        if install.existing || (install.initialized() && !marker.is_file()) {
            return Err(local_error("existing_installation"));
        }
        if !marker.exists() {
            save_private_json(&marker, &TrialMetadata::default())?;
        }
        let metadata: TrialMetadata = read_private_json(&marker)?;
        if install.managed_db.is_none() {
            install.managed_db =
                Some(local_postgres::LocalPostgres::open(&install.dir).map_err(database_error)?);
        }
        let url = install
            .managed_db
            .as_mut()
            .ok_or_else(|| local_error("database_unavailable"))?
            .start()
            .await
            .map_err(database_error)?;
        (
            install.dir.clone(),
            url,
            install.dir.join("initializing").is_file(),
            install.initialized(),
            !metadata.complete,
        )
    };
    if initializing && !initialized {
        archive_incomplete_trial(&mut *state.install.lock().await, &database_url).await?;
    }
    if !initialized {
        set_phase(state, "initializing");
        let result = initialize(
            State(state.clone()),
            Json(Setup {
                database_url,
                managed_local: true,
                local_development: true,
                corpus_path: String::new(),
                login_id: "local-admin".to_owned(),
                display_name: "Local Administrator".to_owned(),
                engine_bind: available_engine_bind()?,
            }),
        )
        .await?;
        let password = result
            .0
            .get("temporary_password")
            .and_then(Value::as_str)
            .ok_or_else(|| local_error("initialization_failed"))?
            .to_owned();
        save_private_json(
            &directory.join(TRIAL_CREDENTIALS_FILE),
            &TrialCredentials {
                login_id: "local-admin".to_owned(),
                password,
                pending_password: None,
                needs_rotation: true,
            },
        )?;
    } else if first && (initializing || !directory.join(TRIAL_CREDENTIALS_FILE).is_file()) {
        set_phase(state, "initializing");
        recover_trial_setup(&mut *state.install.lock().await).await?;
    }
    let session = {
        let mut install = state.install.lock().await;
        start_installation(state, &mut install).await?;
        trial_session(state, &mut install).await?
    };
    let mut metadata: TrialMetadata = read_private_json(&directory.join(TRIAL_METADATA_FILE))?;
    metadata.complete = true;
    save_private_json(&directory.join(TRIAL_METADATA_FILE), &metadata)?;
    let credentials: TrialCredentials =
        read_private_json(&directory.join(TRIAL_CREDENTIALS_FILE))?;
    let mut response = json!({"ok":true,"login_id":credentials.login_id,
        "access_token":session["access_token"],"refresh_token":session["refresh_token"],"expires_in":session["expires_in"]});
    if first {
        response["initial_password"] = json!(credentials.password);
    }
    Ok(response)
}

async fn archive_incomplete_trial(install: &mut Installation, database_url: &str) -> WebResult<()> {
    let metadata: TrialMetadata = read_private_json(&install.dir.join(TRIAL_METADATA_FILE))?;
    let database = install
        .managed_db
        .as_ref()
        .ok_or_else(|| local_error("recovery_required"))?;
    if metadata.complete
        || install.config_path.exists()
        || database.application_database_url() != database_url
    {
        return Err(local_error("recovery_required"));
    }
    let pool = connect(database_url).await?;
    let empty = ensure_empty(&pool).await;
    pool.close().await;
    empty.map_err(|_| local_error("recovery_required"))?;
    let archive = install
        .dir
        .join(format!("incomplete-setup-{}", Uuid::now_v7()));
    private_dir(&archive).map_err(io_error)?;
    // Only pre-configuration metadata from this verified, still-empty local
    // trial is moved aside. PostgreSQL files and existing installations remain intact.
    for filename in [
        "initializing",
        LOCAL_CORPUS_FILE,
        "secrets.json",
        "input-rules.json",
        INITIAL_ADMIN_PASSWORD_FILE,
    ] {
        let source = install.dir.join(filename);
        if source.exists() {
            let metadata = std::fs::symlink_metadata(&source).map_err(io_error)?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(local_error("private_storage"));
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt as _;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(local_error("private_storage"));
                }
            }
            std::fs::rename(&source, archive.join(filename)).map_err(io_error)?;
        }
    }
    Ok(())
}

async fn recover_trial_setup(install: &mut Installation) -> WebResult<()> {
    let database = install
        .managed_db
        .as_ref()
        .ok_or_else(|| local_error("recovery_required"))?;
    let pool = connect(&database.migration_database_url()).await?;
    let result = run_migrations(&pool).await;
    pool.close().await;
    result.map_err(|_| local_error("migration_failed"))?;
    database
        .grant_runtime_access()
        .await
        .map_err(database_error)?;
    let config = install.config()?;
    let env = install.env()?;
    let corpus = env
        .get(PASSWORD_DENYLIST_ENV)
        .ok_or_else(|| local_error("corpus_missing"))?;
    let policy = PasswordPolicy::production(
        read_password_corpus(Path::new(&corpus)).map_err(|_| local_error("corpus_invalid"))?,
    )
    .map_err(|_| local_error("corpus_invalid"))?
    .with_min_length(config.auth.password_min_length);
    let passwords = PasswordService::new_with_argon2_and_concurrency(
        policy,
        config.auth.argon2_memory_cost_kib,
        config.auth.argon2_iterations,
        config.auth.argon2_parallelism,
        config.auth.password_hash_concurrency as usize,
    )
    .map_err(|_| local_error("configuration_invalid"))?;
    let pool = connect(
        &env.get(&config.database.url_env)
            .ok_or_else(|| local_error("database_unavailable"))?,
    )
    .await?;
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(&pool)
        .await
        .map_err(|_| local_error("migration_failed"))?;
    let service = IdentityAdministrationService::new(
        Arc::new(IdentityAdministrationStore::new(pool.clone())),
        Arc::new(SystemClock::new()),
        passwords,
    );
    let request_id = orbisync_application::RequestId::new(format!("req_{}", UserId::generate()))
        .map_err(|_| local_error("initialization_failed"))?;
    let login = LoginId::new("local-admin").map_err(|_| local_error("initialization_failed"))?;
    let password = if count == 0 {
        service
            .bootstrap_administrator(login, "Local Administrator".to_owned(), request_id)
            .await
            .map_err(|_| local_error("initialization_failed"))?
            .1
    } else {
        let repository = PgIdentityRepository::new(pool.clone());
        service
            .recover_local_administrator(&repository, &login, request_id)
            .await
            .map_err(|_| local_error("recovery_required"))?
    };
    save_private_json(
        &install.dir.join(TRIAL_CREDENTIALS_FILE),
        &TrialCredentials {
            login_id: "local-admin".to_owned(),
            password: password.expose_secret().to_owned(),
            pending_password: None,
            needs_rotation: true,
        },
    )?;
    let marker = install.dir.join("initializing");
    if marker.is_file() {
        std::fs::remove_file(marker).map_err(io_error)?;
    }
    pool.close().await;
    Ok(())
}

async fn login_trial(
    state: &WebState,
    install: &mut Installation,
    login: &str,
    password: &str,
) -> WebResult<Value> {
    let url = verified_engine_url(state, install).await?;
    let response = state
        .client
        .post(format!("{url}/v1/auth/login"))
        .json(&json!({"login_id":login,"password":password}))
        .send()
        .await
        .map_err(|_| local_error("engine_unavailable"))?;
    if !response.status().is_success() {
        return Err(local_error("trial_credentials_changed"));
    }
    response
        .json()
        .await
        .map_err(|_| local_error("authentication_failed"))
}
async fn trial_access_token(state: &WebState, install: &mut Installation) -> WebResult<String> {
    trial_session(state, install).await?
        .get("access_token").and_then(Value::as_str).map(str::to_owned)
        .ok_or_else(|| local_error("authentication_failed"))
}
async fn trial_session(state: &WebState, install: &mut Installation) -> WebResult<Value> {
    let path = install.dir.join(TRIAL_CREDENTIALS_FILE);
    if !path.is_file() {
        return Err(local_error("trial_credentials_missing"));
    }
    let mut credentials: TrialCredentials = read_private_json(&path)?;
    if let Some(pending) = credentials.pending_password.as_ref()
        && let Ok(session) = login_trial(state, install, &credentials.login_id, pending).await
    {
        credentials.password = pending.clone();
        credentials.pending_password = None;
        credentials.needs_rotation = false;
        save_private_json(&path, &credentials)?;
        replace(
            &install.dir.join(INITIAL_ADMIN_PASSWORD_FILE),
            credentials.password.as_bytes(),
        )
        .map_err(io_error)?;
        return Ok(session);
    }
    let mut session =
        login_trial(state, install, &credentials.login_id, &credentials.password).await?;
    if credentials.needs_rotation
        || session.get("must_change_password").and_then(Value::as_bool) == Some(true)
    {
        let new_password = credentials
            .pending_password
            .clone()
            .unwrap_or_else(random_secret);
        credentials.pending_password = Some(new_password.clone());
        save_private_json(&path, &credentials)?;
        let token = session
            .get("access_token")
            .and_then(Value::as_str)
            .ok_or_else(|| local_error("authentication_failed"))?;
        let url = verified_engine_url(state, install).await?;
        let response = state
            .client
            // allow-hardcoded-secret: authentication route, never credential data.
            .post(format!("{url}/v1/auth/change-password"))
            .bearer_auth(token)
            .header("Idempotency-Key", Uuid::now_v7().to_string())
            .json(&json!({"current_password":credentials.password,"new_password":new_password}))
            .send()
            .await
            .map_err(|_| local_error("authentication_failed"))?;
        if !response.status().is_success() {
            return Err(local_error("authentication_failed"));
        }
        credentials.password = new_password;
        credentials.pending_password = None;
        credentials.needs_rotation = false;
        save_private_json(&path, &credentials)?;
        // Keep the operator's existing private recovery file current too.
        replace(
            &install.dir.join(INITIAL_ADMIN_PASSWORD_FILE),
            credentials.password.as_bytes(),
        )
        .map_err(io_error)?;
        session = login_trial(state, install, &credentials.login_id, &credentials.password).await?;
    }
    replace(
        &install.dir.join(INITIAL_ADMIN_PASSWORD_FILE),
        credentials.password.as_bytes(),
    )
    .map_err(io_error)?;
    Ok(session)
}

async fn diagnostics(State(state): State<Arc<WebState>>) -> WebResult<Json<Value>> {
    let operation = state
        .operation
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let Ok(mut install) = state.install.try_lock() else {
        return Ok(Json(
            json!({"checks":[{"code":"operation_in_progress","status":"skipped"}],"actions":[]}),
        ));
    };
    let mut checks = Vec::new();
    let mut actions = vec![json!({"action":"retry"})];
    let managed = install.dir.join(TRIAL_METADATA_FILE).is_file();
    let runtime = std::env::current_exe().ok().and_then(|path| {
        path.parent()
            .map(|directory| directory.join("runtime/postgres/BUILD.json"))
    });
    let runtime_available = runtime.as_ref().is_some_and(|path| path.is_file());
    checks.push(json!({"code":if runtime_available {"runtime_available"} else {"runtime_missing"},"status":if managed && !runtime_available {"failed"} else {"passed"}}));
    if install.dir.join("initializing").is_file() {
        checks.push(json!({"code":"initialization_required","status":"failed"}));
    }
    if install.initialized() {
        match install.config() {
            Ok(config) => {
                checks.push(json!({"code":"configuration_valid","status":"passed"}));
                let running = install
                    .engine
                    .as_mut()
                    .is_some_and(|engine| engine.try_wait().is_ok_and(|status| status.is_none()));
                let port_free = loopback_bind(&config.server.bind)
                    .ok()
                    .and_then(|address| std::net::TcpListener::bind(address).ok())
                    .is_some();
                checks.push(json!({"code":if running || port_free {"engine_port_available"} else {"engine_port_in_use"},"status":if running || port_free {"passed"} else {"failed"}}));
                if !install.existing && !running && !port_free {
                    actions.push(json!({"action":"choose_engine_port"}));
                }
                if let Ok(env) = install.env() {
                    let secrets_ok = config.verify_secrets(&env).is_ok()
                        && create_token_service(&env, &config).is_ok();
                    checks.push(json!({"code":if secrets_ok {"keys_valid"} else {"signing_key_invalid"},"status":if secrets_ok {"passed"} else {"failed"}}));
                    let corpus_ok = env
                        .get(PASSWORD_DENYLIST_ENV)
                        .and_then(|path| read_password_corpus(Path::new(&path)).ok())
                        .is_some_and(|values| PasswordPolicy::production(values).is_ok());
                    checks.push(json!({"code":if corpus_ok {"corpus_valid"} else {"corpus_invalid"},"status":if corpus_ok {"passed"} else {"failed"}}));
                    let connected = match env.get(&config.database.url_env) {
                        Some(url) => match connect(&url).await {
                            Ok(pool) => {
                                pool.close().await;
                                true
                            }
                            Err(_) => false,
                        },
                        None => false,
                    };
                    checks.push(json!({"code":if connected {"database_ready"} else {"database_unavailable"},"status":if connected {"passed"} else {"failed"}}));
                }
            }
            Err(_) => checks.push(json!({"code":"configuration_invalid","status":"failed"})),
        }
    } else {
        checks.push(json!({"code":"initialization_required","status":"failed"}));
    }
    if managed
        && matches!(
            operation.get("error_code").and_then(Value::as_str),
            Some("port_in_use" | "database_identity_mismatch")
        )
    {
        actions.push(json!({"action":"choose_database_port"}));
    }
    if let Some(code) = operation.get("error_code").and_then(Value::as_str) {
        checks.push(json!({"code":code,"status":"failed"}));
    }
    Ok(Json(json!({"checks":checks,"actions":actions})))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Repair {
    action: String,
}
async fn repair(
    State(state): State<Arc<WebState>>,
    Json(body): Json<Repair>,
) -> WebResult<Json<Value>> {
    let _operation = LocalOperation::begin(&state, "db_starting")?;
    let result = async {
        match body.action.as_str() {
            "retry" => {
                let is_trial = state
                    .install
                    .lock()
                    .await
                    .dir
                    .join(TRIAL_METADATA_FILE)
                    .is_file();
                if is_trial {
                    create_or_resume_trial(&state).await
                } else {
                    start_installation(&state, &mut *state.install.lock().await)
                        .await
                        .map(|()| json!({"ok":true}))
                }
            }
            "choose_engine_port" => {
                let mut install = state.install.lock().await;
                if install.existing {
                    return Err(local_error("existing_installation"));
                }
                stop_installation(&state, &mut install).await?;
                let mut table: toml::Table = std::fs::read_to_string(&install.config_path)
                    .map_err(io_error)?
                    .parse()
                    .map_err(io_error)?;
                table
                    .entry("server")
                    .or_insert(toml::Value::Table(toml::Table::new()))
                    .as_table_mut()
                    .ok_or_else(|| local_error("configuration_invalid"))?
                    .insert("bind".into(), toml::Value::String(available_engine_bind()?));
                replace(
                    &install.config_path,
                    toml::to_string_pretty(&table).map_err(io_error)?.as_bytes(),
                )
                .map_err(io_error)?;
                start_installation(&state, &mut install).await?;
                Ok(json!({"ok":true}))
            }
            "choose_database_port" => {
                let mut install = state.install.lock().await;
                if install.existing || !install.dir.join(TRIAL_METADATA_FILE).is_file() {
                    return Err(local_error("existing_installation"));
                }
                stop_installation(&state, &mut install).await?;
                if install.managed_db.is_none() {
                    install.managed_db = Some(
                        local_postgres::LocalPostgres::open(&install.dir)
                            .map_err(database_error)?,
                    );
                }
                let url = install
                    .managed_db
                    .as_mut()
                    .ok_or_else(|| local_error("database_unavailable"))?
                    .repair_port()
                    .await
                    .map_err(database_error)?;
                let config = install.config()?;
                let path = install.dir.join("secrets.json");
                let mut secrets: BTreeMap<String, String> = read_private_json(&path)?;
                secrets.insert(config.database.url_env, url);
                save_private_json(&path, &secrets)?;
                start_installation(&state, &mut install).await?;
                Ok(json!({"ok":true}))
            }
            _ => Err(local_error("unsupported_action")),
        }
    }
    .await;
    complete_operation(&state, &result, "ready");
    result.map(Json)
}

async fn engine_request(
    state: &WebState,
    install: &mut Installation,
    token: &str,
    method: reqwest::Method,
    path: &str,
    body: Option<Value>,
) -> WebResult<Value> {
    let url = verified_engine_url(state, install).await?;
    let mut request = state
        .client
        .request(method, format!("{url}/v1/{path}"))
        .bearer_auth(token)
        .header("Idempotency-Key", Uuid::now_v7().to_string());
    if let Some(body) = body {
        request = request.json(&body);
    }
    let response = request
        .send()
        .await
        .map_err(|_| local_error("engine_unavailable"))?;
    if !response.status().is_success() {
        return Err(local_error("probe_request_failed"));
    }
    let bytes = response
        .bytes()
        .await
        .map_err(|_| local_error("probe_request_failed"))?;
    if bytes.is_empty() {
        Ok(Value::Null)
    } else {
        serde_json::from_slice(&bytes).map_err(|_| local_error("probe_request_failed"))
    }
}

async fn probe_instance(
    state: &WebState,
    install: &mut Installation,
    token: &str,
) -> WebResult<String> {
    let path = install.dir.join(TRIAL_METADATA_FILE);
    if !path.is_file() {
        return Err(local_error("probe_requires_local_trial"));
    }
    let mut metadata: TrialMetadata = read_private_json(&path)?;
    if metadata.world_id.is_none() {
        let result = engine_request(
            state,
            install,
            token,
            reqwest::Method::POST,
            "worlds",
            Some(json!({"name":"Local connection test","capacity":4})),
        )
        .await?;
        metadata.world_id = result.get("id").and_then(Value::as_str).map(str::to_owned);
        if metadata.world_id.is_none() {
            return Err(local_error("probe_request_failed"));
        }
        save_private_json(&path, &metadata)?;
    }
    if metadata.instance_id.is_none() {
        let result = engine_request(
            state,
            install,
            token,
            reqwest::Method::POST,
            "instances",
            Some(json!({"world_id":metadata.world_id})),
        )
        .await?;
        metadata.instance_id = result.get("id").and_then(Value::as_str).map(str::to_owned);
        if metadata.instance_id.is_none() {
            return Err(local_error("probe_request_failed"));
        }
        save_private_json(&path, &metadata)?;
    }
    let id = metadata
        .instance_id
        .ok_or_else(|| local_error("probe_request_failed"))?;
    let instance = engine_request(
        state,
        install,
        token,
        reqwest::Method::GET,
        &format!("instances/{id}"),
        None,
    )
    .await?;
    if instance.get("status").and_then(Value::as_str) != Some("running") {
        engine_request(
            state,
            install,
            token,
            reqwest::Method::POST,
            &format!("instances/{id}/start"),
            None,
        )
        .await?;
    }
    Ok(id)
}

async fn probe_connection(
    State(state): State<Arc<WebState>>,
    headers: HeaderMap,
) -> WebResult<Json<Value>> {
    let _operation = LocalOperation::begin(&state, "probing")?;
    let mut checks = vec![
        json!({"step":"http","status":"skipped"}),
        json!({"step":"websocket","status":"skipped"}),
        json!({"step":"join","status":"skipped"}),
        json!({"step":"sync","status":"skipped"}),
    ];
    let mut step = 0usize;
    let mut owned_token: Option<String> = None;
    let result: WebResult<()> = async {
        let mut install = state.install.lock().await;
        let url = verified_engine_url(&state, &mut install).await?;
        let ready = state
            .client
            .get(format!("{url}/health/ready"))
            .send()
            .await
            .map_err(|_| local_error("engine_unavailable"))?;
        if !ready.status().is_success() {
            return Err(local_error("engine_unavailable"));
        }
        let token = if let Some(token) = headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        {
            token
        } else {
            owned_token = Some(trial_access_token(&state, &mut install).await?);
            owned_token
                .as_deref()
                .ok_or_else(|| local_error("authentication_failed"))?
        };
        // Probe fixtures are only created inside this explicitly managed local
        // trial. Ordinary external databases are never seeded by diagnostics.
        let id = probe_instance(&state, &mut install, token).await?;
        let ticket = engine_request(
            &state,
            &mut install,
            token,
            reqwest::Method::POST,
            "realtime/tickets",
            Some(json!({})),
        )
        .await?;
        let ticket = ticket
            .get("realtime_ticket")
            .and_then(Value::as_str)
            .ok_or_else(|| local_error("ticket_failed"))?;
        checks[0]["status"] = json!("passed");
        step = 1;
        use futures_util::{SinkExt as _, StreamExt as _};
        use prost::Message as _;
        use tokio_tungstenite::tungstenite::{
            Message, client::IntoClientRequest as _, http::header::SEC_WEBSOCKET_PROTOCOL,
        };
        let mut request = format!("{}/v1/realtime/ws", url.replacen("http://", "ws://", 1))
            .into_client_request()
            .map_err(|_| local_error("websocket_failed"))?;
        request.headers_mut().insert(
            SEC_WEBSOCKET_PROTOCOL,
            orbisync_protocol::WEBSOCKET_SUBPROTOCOL
                .parse()
                .map_err(|_| local_error("websocket_failed"))?,
        );
        let (mut socket, response) = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio_tungstenite::connect_async(request),
        )
        .await
        .map_err(|_| local_error("websocket_timeout"))?
        .map_err(|_| local_error("websocket_failed"))?;
        if response
            .headers()
            .get(SEC_WEBSOCKET_PROTOCOL)
            .and_then(|value| value.to_str().ok())
            != Some(orbisync_protocol::WEBSOCKET_SUBPROTOCOL)
        {
            return Err(local_error("websocket_failed"));
        }
        let envelope = |sequence, instance_id, payload| orbisync_protocol::v1::Envelope {
            protocol_major: orbisync_protocol::PROTOCOL_MAJOR,
            protocol_minor: orbisync_protocol::version::SUPPORTED_MINOR_MAX,
            message_id: Uuid::now_v7().to_string(),
            sequence,
            sent_at_unix_ms: SystemClock::new().now().to_unix_millis().unwrap_or(0),
            instance_id,
            payload: Some(payload),
        };
        use orbisync_protocol::v1::{ClientHello, JoinInstance, envelope::Payload};
        socket
            .send(Message::Binary(
                envelope(
                    1,
                    String::new(),
                    Payload::ClientHello(ClientHello {
                        realtime_ticket: ticket.to_owned(),
                        supported_minor_min: 0,
                        supported_minor_max: orbisync_protocol::version::SUPPORTED_MINOR_MAX,
                        client_name: "orbisync-local-probe".to_owned(),
                        client_version: VERSION.to_owned(),
                        client_type: "operator".to_owned(),
                        supported_features: vec!["orbisync.state-sync.v1".to_owned()],
                        ..Default::default()
                    }),
                )
                .encode_to_vec()
                .into(),
            ))
            .await
            .map_err(|_| local_error("websocket_failed"))?;
        let hello = tokio::time::timeout(std::time::Duration::from_secs(5), socket.next())
            .await
            .map_err(|_| local_error("websocket_timeout"))?
            .ok_or_else(|| local_error("websocket_failed"))?
            .map_err(|_| local_error("websocket_failed"))?;
        let Message::Binary(bytes) = hello else {
            return Err(local_error("websocket_failed"));
        };
        let hello = orbisync_protocol::v1::Envelope::decode(bytes)
            .map_err(|_| local_error("protocol_invalid"))?;
        if !matches!(hello.payload, Some(Payload::ServerHello(_))) {
            return Err(local_error("protocol_invalid"));
        }
        checks[1]["status"] = json!("passed");
        step = 2;
        socket
            .send(Message::Binary(
                envelope(
                    2,
                    id.clone(),
                    Payload::JoinInstance(JoinInstance {
                        world_instance_id: id.clone(),
                    }),
                )
                .encode_to_vec()
                .into(),
            ))
            .await
            .map_err(|_| local_error("join_failed"))?;
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut joined = false;
        let mut snapshot_id = String::new();
        let mut chunks = BTreeMap::new();
        let mut chunk_count = 0u32;
        let mut snapshot_revision = None;
        let mut total = 0usize;
        loop {
            let message = tokio::time::timeout_at(deadline, socket.next())
                .await
                .map_err(|_| local_error("sync_timeout"))?
                .ok_or_else(|| local_error("sync_failed"))?
                .map_err(|_| local_error("sync_failed"))?;
            let Message::Binary(bytes) = message else {
                continue;
            };
            let frame = orbisync_protocol::v1::Envelope::decode(bytes)
                .map_err(|_| local_error("protocol_invalid"))?;
            match frame.payload {
                Some(Payload::JoinAccepted(join)) if join.instance_id == id => {
                    joined = true;
                    checks[2]["status"] = json!("passed");
                    step = 3;
                }
                Some(Payload::Snapshot(snapshot)) if joined => {
                    if snapshot.chunk_count == 0
                        || snapshot.chunk_count > 256
                        || snapshot.chunk_index >= snapshot.chunk_count
                    {
                        return Err(local_error("protocol_invalid"));
                    }
                    if snapshot_id.is_empty() {
                        snapshot_id = snapshot.snapshot_id.clone();
                        chunk_count = snapshot.chunk_count;
                        snapshot_revision = Some(snapshot.instance_revision);
                    }
                    if snapshot.snapshot_id != snapshot_id
                        || snapshot.chunk_count != chunk_count
                        || snapshot_revision != Some(snapshot.instance_revision)
                        || chunks.contains_key(&snapshot.chunk_index)
                    {
                        return Err(local_error("protocol_invalid"));
                    }
                    total = total.saturating_add(snapshot.data.len());
                    if total > 4 * 1024 * 1024 {
                        return Err(local_error("probe_snapshot_too_large"));
                    }
                    chunks.insert(snapshot.chunk_index, snapshot.data);
                    if chunks.len() == chunk_count as usize {
                        let data: Vec<u8> = chunks.into_values().flatten().collect();
                        let value: Value = serde_json::from_slice(&data)
                            .map_err(|_| local_error("protocol_invalid"))?;
                        if value.get("format").and_then(Value::as_str)
                            != Some("orbisync.snapshot.v1")
                            || value
                                .get("instance")
                                .and_then(|instance| instance.get("instance_id"))
                                .and_then(Value::as_str)
                                != Some(id.as_str())
                            || !value.get("entities").is_some_and(Value::is_array)
                            || !value.get("users").is_some_and(Value::is_array)
                            || value.get("revision").and_then(|value| {
                                value
                                    .as_u64()
                                    .or_else(|| value.as_str().and_then(|value| value.parse().ok()))
                            }) != snapshot_revision
                        {
                            return Err(local_error("protocol_invalid"));
                        }
                        checks[3]["status"] = json!("passed");
                        break;
                    }
                }
                Some(Payload::Error(_)) => return Err(local_error("probe_request_failed")),
                _ => {}
            }
        }
        // Closing the actual socket exercises the ordinary presence cleanup.
        socket
            .close(None)
            .await
            .map_err(|_| local_error("websocket_failed"))?;
        Ok(())
    }
    .await;
    if let Some(token) = owned_token {
        let mut install = state.install.lock().await;
        if engine_request(
            &state,
            &mut install,
            &token,
            reqwest::Method::POST,
            "auth/logout",
            None,
        )
        .await
        .is_err()
        {
            tracing::debug!(event = "local_probe.session_cleanup_unavailable");
        }
    }
    if let Err((_, body)) = &result {
        checks[step]["status"] = json!("failed");
        checks[step]["code"] = body.0.get("code").cloned().unwrap_or(json!("probe_failed"));
    }
    complete_operation(
        &state,
        &result,
        if result.is_ok() { "ready" } else { "error" },
    );
    Ok(Json(json!({"ok":result.is_ok(),"checks":checks})))
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod local_tests {
    use super::*;
    #[test]
    fn private_trial_metadata_never_contains_credentials() {
        let value = serde_json::to_value(TrialMetadata::default()).expect("metadata");
        assert_eq!(
            value,
            json!({"complete":false,"world_id":null,"instance_id":null})
        );
        assert!(!value.to_string().contains("password"));
    }
    #[test]
    fn engine_port_selection_is_loopback_and_never_reuses_a_listener() {
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").expect("owned listener");
        let selected = available_engine_bind().expect("available bind");
        let address: std::net::SocketAddr = selected.parse().expect("socket address");
        assert!(address.ip().is_loopback());
        assert_ne!(
            address.port(),
            occupied.local_addr().expect("owned address").port()
        );
    }
}
