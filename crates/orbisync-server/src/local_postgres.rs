//! A private, bundled PostgreSQL cluster owned by one local launcher.
//!
//! No system PostgreSQL service, PATH lookup, arbitrary PID signalling, Docker,
//! or external database initialization is used here. Credentials never appear
//! in process arguments, diagnostics, or the public status representation.

use std::{
    fs::{File, OpenOptions},
    io::Write as _,
    net::{Ipv4Addr, TcpListener},
    path::{Path, PathBuf},
    process::{Child, Command, ExitStatus, Stdio},
    time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use sqlx::{
    ConnectOptions as _, Connection as _, PgConnection, Row as _,
    postgres::{PgConnectOptions, PgSslMode},
};

const FORMAT_VERSION: u32 = 1;
const POSTGRES_MAJOR: u32 = 17;
const OWNER_ROLE: &str = "orbisync_local_owner";
const APP_ROLE: &str = "orbisync";
const APP_DATABASE: &str = "orbisync";
const START_TIMEOUT: Duration = Duration::from_secs(45);
const CONNECTION_TIMEOUT: Duration = Duration::from_secs(3);

/// A stable, non-secret diagnosis suitable for local UI output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub(super) enum LocalPostgresError {
    #[error("Bundled PostgreSQL runtime is missing or incomplete.")]
    RuntimeMissing,
    #[error("Bundled PostgreSQL major version does not match this installation.")]
    VersionMismatch,
    #[error("The private PostgreSQL directory cannot be read or protected.")]
    PrivateStorage,
    #[error("Another launcher owns this PostgreSQL installation.")]
    InUse,
    #[error("An unrecognized or incomplete database was preserved. Local recovery is required.")]
    RecoveryRequired,
    #[error(
        "The managed PostgreSQL identity could not be verified. No existing process was stopped."
    )]
    IdentityMismatch,
    #[error(
        "The saved PostgreSQL port is occupied. Close its owner or explicitly repair the local installation."
    )]
    PortInUse,
    #[error(
        "PostgreSQL initialization failed. Check runtime dependencies and directory permissions, then retry."
    )]
    InitializationFailed,
    #[error(
        "PostgreSQL could not start. Check runtime dependencies and local permissions, then retry."
    )]
    StartFailed,
    #[error(
        "PostgreSQL did not become ready before the startup deadline. Retry after checking local resources."
    )]
    ReadinessTimeout,
    #[error("Managed database provisioning failed. Its files were preserved for retry.")]
    ProvisioningFailed,
    #[error("Managed PostgreSQL did not stop cleanly. No unrelated process was stopped.")]
    StopFailed,
}

impl LocalPostgresError {
    pub(super) fn code(self) -> &'static str {
        match self {
            Self::RuntimeMissing => "runtime_missing",
            Self::VersionMismatch => "version_mismatch",
            Self::PrivateStorage => "private_storage",
            Self::InUse => "in_use",
            Self::RecoveryRequired => "recovery_required",
            Self::IdentityMismatch => "identity_mismatch",
            Self::PortInUse => "port_in_use",
            Self::InitializationFailed => "initialization_failed",
            Self::StartFailed => "start_failed",
            Self::ReadinessTimeout => "readiness_timeout",
            Self::ProvisioningFailed => "provisioning_failed",
            Self::StopFailed => "stop_failed",
        }
    }
}

/// Status contains no database URL, password, SQL, stderr, or filesystem path.
#[derive(Debug, Clone, Serialize)]
pub(super) struct LocalPostgresStatus {
    pub state: &'static str,
    pub port: Option<u16>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    postgres_major: u32,
    installation_id: uuid::Uuid,
    port: u16,
    owner_password: String,
    application_password: String,
}

impl Manifest {
    fn valid(&self) -> bool {
        self.version == FORMAT_VERSION
            && self.postgres_major == POSTGRES_MAJOR
            && !self.installation_id.is_nil()
            && self.port != 0
            && [
                self.owner_password.as_str(),
                self.application_password.as_str(),
            ]
            .into_iter()
            .all(|password| {
                password.len() == 43
                    && password
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            })
            && self.owner_password != self.application_password
    }

    fn application_url(&self) -> String {
        format!(
            "postgres://{APP_ROLE}:{}@127.0.0.1:{}/{APP_DATABASE}?sslmode=disable",
            self.application_password, self.port
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PostmasterIdentity {
    pid: u32,
    started_seconds: u64,
}

struct Running {
    // Some only when this launcher spawned this exact process. Emergency kill
    // is restricted to its Child handle; adopted processes require SQL identity
    // verification and an explicit graceful stop.
    child: Option<Child>,
    identity: PostmasterIdentity,
}

/// Owns the exclusive launcher lock and the managed PostgreSQL lifetime.
pub(super) struct LocalPostgres {
    root: PathBuf,
    data: PathBuf,
    runtime: PathBuf,
    manifest: Manifest,
    _lock: File,
    running: Option<Running>,
    stage: &'static str,
    path_encodings: std::sync::Mutex<Vec<Vec<u8>>>,
    #[cfg(all(test, windows))]
    force_restricted_start: bool,
}

impl std::fmt::Debug for LocalPostgres {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LocalPostgres")
            .field("status", &self.status())
            .finish_non_exhaustive()
    }
}

impl LocalPostgres {
    /// Whether this directory has an owned local-cluster manifest. This never
    /// treats an arbitrary PGDATA or external connection URL as managed.
    pub fn managed(install_dir: &Path) -> bool {
        install_dir
            .join("local-postgres")
            .join("managed.json")
            .is_file()
    }

    /// Opens or prepares only `<installation>/local-postgres`, taking an OS
    /// lock before inspecting or changing private cluster metadata.
    pub fn open(install_dir: &Path) -> Result<Self, LocalPostgresError> {
        let executable = std::env::current_exe().map_err(|_| LocalPostgresError::RuntimeMissing)?;
        let runtime = executable
            .parent()
            .ok_or(LocalPostgresError::RuntimeMissing)?
            .join("runtime/postgres");
        Self::open_with_runtime(install_dir, &runtime)
    }

    fn open_with_runtime(install_dir: &Path, runtime: &Path) -> Result<Self, LocalPostgresError> {
        validate_runtime(runtime)?;
        Self::open_storage(install_dir, runtime)
    }

    fn open_storage(install_dir: &Path, runtime: &Path) -> Result<Self, LocalPostgresError> {
        crate::web_admin::private_dir(install_dir)
            .map_err(|_| LocalPostgresError::PrivateStorage)?;
        let install = canonical(install_dir)?;
        if install.to_string_lossy().chars().any(char::is_control) {
            return Err(LocalPostgresError::PrivateStorage);
        }
        let root = install.join("local-postgres");
        crate::web_admin::private_dir(&root).map_err(|_| LocalPostgresError::PrivateStorage)?;
        let lock_path = root.join("controller.lock");
        reject_link(&lock_path)?;
        let mut options = OpenOptions::new();
        options.create(true).truncate(false).read(true).write(true);
        private_options(&mut options);
        let lock = options
            .open(&lock_path)
            .map_err(|_| LocalPostgresError::PrivateStorage)?;
        lock.try_lock().map_err(|_| LocalPostgresError::InUse)?;
        let path = root.join("managed.json");
        let manifest = if path.exists() {
            reject_link(&path)?;
            let bytes = std::fs::read(&path).map_err(|_| LocalPostgresError::PrivateStorage)?;
            if bytes.len() > 4096 {
                return Err(LocalPostgresError::RecoveryRequired);
            }
            let manifest: Manifest =
                serde_json::from_slice(&bytes).map_err(|_| LocalPostgresError::RecoveryRequired)?;
            if !manifest.valid() {
                return Err(LocalPostgresError::RecoveryRequired);
            }
            manifest
        } else {
            if std::fs::read_dir(&root)
                .map_err(|_| LocalPostgresError::PrivateStorage)?
                .any(|entry| entry.map_or(true, |entry| entry.file_name() != "controller.lock"))
            {
                return Err(LocalPostgresError::RecoveryRequired);
            }
            let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
                .map_err(|_| LocalPostgresError::PortInUse)?;
            let port = reservation
                .local_addr()
                .map_err(|_| LocalPostgresError::PortInUse)?
                .port();
            let manifest = Manifest {
                version: FORMAT_VERSION,
                postgres_major: POSTGRES_MAJOR,
                installation_id: uuid::Uuid::now_v7(),
                port,
                owner_password: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
                application_password: URL_SAFE_NO_PAD.encode(rand::random::<[u8; 32]>()),
            };
            write_new(
                &path,
                &serde_json::to_vec(&manifest).map_err(|_| LocalPostgresError::PrivateStorage)?,
            )?;
            manifest
        };
        let data = root.join("data");
        if data.exists() {
            crate::web_admin::private_dir(&data).map_err(|_| LocalPostgresError::PrivateStorage)?;
        }
        Ok(Self {
            root,
            data,
            runtime: canonical(runtime)?,
            manifest,
            _lock: lock,
            running: None,
            stage: "stopped",
            path_encodings: std::sync::Mutex::new(Vec::new()),
            #[cfg(all(test, windows))]
            force_restricted_start: false,
        })
    }

    pub fn status(&self) -> LocalPostgresStatus {
        LocalPostgresStatus {
            state: self.stage,
            port: Some(self.manifest.port),
        }
    }

    /// The non-superuser connection URL for private engine settings only.
    pub fn application_database_url(&self) -> String {
        self.manifest.application_url()
    }

    /// Elevated URL for the explicit migration phase only. Never persist it in
    /// engine settings or return it through the local HTTP API.
    pub fn migration_database_url(&self) -> String {
        format!(
            "postgres://{OWNER_ROLE}:{}@127.0.0.1:{}/{APP_DATABASE}?sslmode=disable",
            self.manifest.owner_password, self.manifest.port
        )
    }

    /// Starts/reuses only this verified private cluster. A saved port is stable
    /// across restarts, because the engine's private settings retain this URL.
    pub async fn start(&mut self) -> Result<String, LocalPostgresError> {
        let result = self.start_inner().await;
        match &result {
            Ok(_) => self.stage = "ready",
            Err(error) => self.stage = error.code(),
        }
        result
    }

    async fn start_inner(&mut self) -> Result<String, LocalPostgresError> {
        self.clear_exited_running()?;
        if self.running.is_some() {
            let observed = self.postmaster_identity()?;
            if self
                .running
                .as_ref()
                .is_some_and(|running| running.child.is_none())
            {
                self.verify_adopted_process(&observed)?;
            }
            let running = self
                .running
                .as_mut()
                .ok_or(LocalPostgresError::IdentityMismatch)?;
            if running.identity.pid != observed.pid
                || (running.identity.started_seconds != 0 && running.identity != observed)
            {
                return Err(LocalPostgresError::IdentityMismatch);
            }
            running.identity = observed;
            let _verified = self.verified_connection().await?;
            self.provision().await?;
            return Ok(self.manifest.application_url());
        }
        self.stage = "initializing";
        self.ensure_cluster().await?;
        if self.data.join("postmaster.pid").exists() {
            if let Ok(identity) = self.adopted_identity() {
                let _verified = self.verified_connection().await?;
                self.running = Some(Running {
                    child: None,
                    identity,
                });
                self.stage = "provisioning";
                self.provision().await?;
                return Ok(self.manifest.application_url());
            }
            // Let postgres itself handle a stale lock file, but never signal a
            // PID based only on an unverified postmaster.pid file.
            let mut status = self.command("pg_ctl");
            status.arg("status").arg("-D").arg(&self.data);
            if run_command(status, Duration::from_secs(5)).await?.code() != Some(3) {
                return Err(LocalPostgresError::IdentityMismatch);
            }
        }
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, self.manifest.port))
            .map_err(|_| LocalPostgresError::PortInUse)?;
        self.write_configuration()?;
        self.stage = "starting";
        #[cfg(windows)]
        if self.windows_restricted_start_required()? {
            drop(reservation);
            return self.start_windows_restricted().await;
        }
        let mut command = self.command("postgres");
        command.arg("-D").arg(&self.data);
        drop(reservation);
        let child = command
            .spawn()
            .map_err(|_| LocalPostgresError::StartFailed)?;
        let child_id = child.id();
        self.running = Some(Running {
            child: Some(child),
            identity: PostmasterIdentity {
                pid: child_id,
                started_seconds: 0,
            },
        });
        let deadline = Instant::now() + START_TIMEOUT;
        loop {
            if self
                .running
                .as_mut()
                .and_then(|running| running.child.as_mut())
                .is_some_and(|child| child.try_wait().ok().flatten().is_some())
            {
                self.running = None;
                return Err(LocalPostgresError::StartFailed);
            }
            if let Ok(identity) = self.postmaster_identity() {
                if identity.pid != child_id {
                    return Err(LocalPostgresError::IdentityMismatch);
                }
                if self.verified_connection().await.is_ok() {
                    if let Some(running) = self.running.as_mut() {
                        running.identity = identity;
                    }
                    break;
                }
            }
            if Instant::now() >= deadline {
                return Err(LocalPostgresError::ReadinessTimeout);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        self.stage = "provisioning";
        self.provision().await?;
        Ok(self.manifest.application_url())
    }

    #[cfg(windows)]
    fn windows_restricted_start_required(&self) -> Result<bool, LocalPostgresError> {
        #[cfg(test)]
        if self.force_restricted_start {
            return Ok(true);
        }
        use std::os::windows::process::CommandExt as _;
        // Match PostgreSQL's pgwin32_is_admin: either enabled group requires
        // the official restricted-token startup, including Power Users.
        let status = Command::new(windows_powershell()?)
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; $p=[System.Security.Principal.WindowsPrincipal]::new([System.Security.Principal.WindowsIdentity]::GetCurrent()); if ($p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator) -or $p.IsInRole([System.Security.Principal.WindowsBuiltInRole]::PowerUser)) { exit 0 }; exit 3"])
            .env_remove("PSModulePath").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .creation_flags(0x08000000).status().map_err(|_| LocalPostgresError::StartFailed)?;
        match status.code() {
            Some(0) => Ok(true),
            Some(3) => Ok(false),
            _ => Err(LocalPostgresError::StartFailed),
        }
    }

    #[cfg(windows)]
    async fn start_windows_restricted(&mut self) -> Result<String, LocalPostgresError> {
        // PostgreSQL refuses an elevated direct CreateProcess launch. pg_ctl
        // creates a restricted token itself, and its transient PID belongs to
        // an ancestor CMD process, so never treat that PID as the server's.
        let mut command = self.command("pg_ctl");
        command
            .arg("start")
            .arg("-D")
            .arg(&self.data)
            .args(["-w", "-t", "45"]);
        if !run_command(command, START_TIMEOUT + Duration::from_secs(10))
            .await?
            .success()
        {
            return Err(LocalPostgresError::StartFailed);
        }
        // No control authority is assumed from pg_ctl success alone. Verify
        // exact OS executable/-D, ready PID/start time, SQL directory/UUID/port.
        let identity = self.adopted_identity()?;
        let _verified = self.verified_connection().await?;
        self.running = Some(Running {
            child: None,
            identity,
        });
        self.stage = "provisioning";
        self.provision().await?;
        Ok(self.manifest.application_url())
    }

    async fn ensure_cluster(&self) -> Result<(), LocalPostgresError> {
        reject_link(&self.data)?;
        let version = self.data.join("PG_VERSION");
        if version.exists() {
            reject_link(&version)?;
            if std::fs::read_to_string(&version)
                .map_err(|_| LocalPostgresError::PrivateStorage)?
                .trim()
                != POSTGRES_MAJOR.to_string()
            {
                return Err(LocalPostgresError::VersionMismatch);
            }
            if !self.data.join("global/pg_control").is_file() {
                return Err(LocalPostgresError::RecoveryRequired);
            }
            // ALTER SYSTEM overrides the normal config. Managed clusters never
            // accept such overrides (including external listen/data/HBA paths).
            let automatic = self.data.join("postgresql.auto.conf");
            reject_link(&automatic)?;
            if automatic.exists()
                && std::fs::read_to_string(&automatic)
                    .map_err(|_| LocalPostgresError::PrivateStorage)?
                    .lines()
                    .any(|line| !line.trim().is_empty() && !line.trim_start().starts_with('#'))
            {
                return Err(LocalPostgresError::RecoveryRequired);
            }
            let tablespaces = self.data.join("pg_tblspc");
            reject_link(&tablespaces)?;
            if tablespaces.exists()
                && std::fs::read_dir(tablespaces)
                    .map_err(|_| LocalPostgresError::PrivateStorage)?
                    .next()
                    .is_some()
            {
                return Err(LocalPostgresError::RecoveryRequired);
            }
            return Ok(());
        }
        if self.data.exists()
            && std::fs::read_dir(&self.data)
                .map_err(|_| LocalPostgresError::PrivateStorage)?
                .next()
                .is_some()
        {
            return Err(LocalPostgresError::RecoveryRequired);
        }
        // allow-hardcoded-secret: filename for a temporary private random-password input, never a password value.
        let password_file = self.root.join("initdb-password.txt");
        if password_file.exists() {
            reject_link(&password_file)?;
            if std::fs::read_to_string(&password_file)
                .map_err(|_| LocalPostgresError::PrivateStorage)?
                != self.manifest.owner_password
            {
                return Err(LocalPostgresError::RecoveryRequired);
            }
        } else {
            write_new(&password_file, self.manifest.owner_password.as_bytes())?;
        }
        let mut command = self.command("initdb");
        command
            .arg("-D")
            .arg(&self.data)
            .arg("--encoding=UTF8")
            .arg("--locale=C")
            .arg("--auth=scram-sha-256")
            .arg(format!("--username={OWNER_ROLE}"))
            .arg("--pwfile")
            .arg(&password_file)
            .arg("--data-checksums");
        let result = run_command(command, Duration::from_secs(120)).await;
        // This temporary file is never used as the persisted credential store.
        let _removed = std::fs::remove_file(&password_file);
        if !result.is_ok_and(|status| status.success()) {
            return Err(LocalPostgresError::InitializationFailed);
        }
        crate::web_admin::private_dir(&self.data)
            .map_err(|_| LocalPostgresError::PrivateStorage)?;
        Ok(())
    }

    fn write_configuration(&self) -> Result<(), LocalPostgresError> {
        let config = format!(
            "# Managed local OrbiSync cluster; loopback TCP with SCRAM only.\nlisten_addresses = '127.0.0.1'\nport = {}\nunix_socket_directories = ''\nssl = off\npassword_encryption = 'scram-sha-256'\nmax_connections = 100\nshared_buffers = '64MB'\nfsync = on\nsynchronous_commit = on\nfull_page_writes = on\nlogging_collector = off\nlog_statement = 'none'\nlog_min_error_statement = 'panic'\nlog_parameter_max_length_on_error = 0\nlog_connections = off\nlog_disconnections = off\norbisync.installation_id = '{}'\n",
            self.manifest.port, self.manifest.installation_id
        );
        replace(&self.data.join("postgresql.conf"), config.as_bytes())?;
        // initdb uses SCRAM already; narrow its generated HBA to the two exact
        // managed roles/databases before the server ever listens.
        replace(&self.data.join("pg_hba.conf"), format!(
            "host postgres,{APP_DATABASE} {OWNER_ROLE} 127.0.0.1/32 scram-sha-256\nhost {APP_DATABASE} {APP_ROLE} 127.0.0.1/32 scram-sha-256\nhost all all 0.0.0.0/0 reject\nhost all all ::/0 reject\nlocal all all reject\n").as_bytes())?;
        Ok(())
    }

    async fn verified_connection(&self) -> Result<PgConnection, LocalPostgresError> {
        let options =
            self.connection_options(OWNER_ROLE, &self.manifest.owner_password, "postgres");
        let mut connection =
            tokio::time::timeout(CONNECTION_TIMEOUT, PgConnection::connect_with(&options))
                .await
                .map_err(|_| LocalPostgresError::IdentityMismatch)?
                .map_err(|_| LocalPostgresError::IdentityMismatch)?;
        let identity = tokio::time::timeout(CONNECTION_TIMEOUT, sqlx::query(
            "SELECT pg_catalog.textsend(current_setting('data_directory')) AS data_directory, current_setting('listen_addresses') AS listen_addresses, current_setting('port') AS port, current_setting('orbisync.installation_id', true) AS installation_id")
            .fetch_one(&mut connection)).await.map_err(|_| LocalPostgresError::IdentityMismatch)?.map_err(|_| LocalPostgresError::IdentityMismatch)?;
        let directory: Vec<u8> = identity
            .try_get("data_directory")
            .map_err(|_| LocalPostgresError::IdentityMismatch)?;
        let listen: String = identity
            .try_get("listen_addresses")
            .map_err(|_| LocalPostgresError::IdentityMismatch)?;
        let port: String = identity
            .try_get("port")
            .map_err(|_| LocalPostgresError::IdentityMismatch)?;
        let installation: Option<String> = identity
            .try_get("installation_id")
            .map_err(|_| LocalPostgresError::IdentityMismatch)?;
        if !self.postgres_path_matches(&directory)?
            || listen != "127.0.0.1"
            || port != self.manifest.port.to_string()
            || installation.as_deref() != Some(self.manifest.installation_id.to_string().as_str())
        {
            return Err(LocalPostgresError::IdentityMismatch);
        }
        Ok(connection)
    }

    fn connection_options(&self, user: &str, password: &str, database: &str) -> PgConnectOptions {
        PgConnectOptions::new()
            .host("127.0.0.1")
            .port(self.manifest.port)
            .username(user)
            .password(password)
            .database(database)
            .ssl_mode(PgSslMode::Disable)
            .application_name("orbisync-local-controller")
            .disable_statement_logging()
    }

    async fn provision(&self) -> Result<(), LocalPostgresError> {
        let mut connection = self.verified_connection().await?;
        let application_exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'orbisync')")
                .fetch_one(&mut connection)
                .await
                .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        let verb = if application_exists {
            "ALTER"
        } else {
            "CREATE"
        };
        // The password is a validated fixed-length URL-safe random value, never
        // user input. PostgreSQL role DDL does not accept bind parameters here.
        // Client statement logging and server error-statement logging are off.
        sqlx::query(&format!("{verb} ROLE {APP_ROLE} LOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOREPLICATION PASSWORD '{}'", self.manifest.application_password))
            .execute(&mut connection).await.map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        let database_exists: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM pg_database WHERE datname = 'orbisync')",
        )
        .fetch_one(&mut connection)
        .await
        .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        if !database_exists {
            sqlx::query(
            "CREATE DATABASE orbisync OWNER orbisync_local_owner TEMPLATE template0 ENCODING 'UTF8'",
            )
            .execute(&mut connection)
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        }
        let owner: String = sqlx::query_scalar(
            "SELECT pg_get_userbyid(datdba) FROM pg_database WHERE datname = 'orbisync'",
        )
        .fetch_one(&mut connection)
        .await
        .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        if owner != OWNER_ROLE {
            return Err(LocalPostgresError::IdentityMismatch);
        }
        sqlx::query("REVOKE ALL ON DATABASE orbisync FROM PUBLIC")
            .execute(&mut connection)
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        sqlx::query("GRANT CONNECT ON DATABASE orbisync TO orbisync")
            .execute(&mut connection)
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        let options =
            self.connection_options(APP_ROLE, &self.manifest.application_password, APP_DATABASE);
        tokio::time::timeout(CONNECTION_TIMEOUT, PgConnection::connect_with(&options))
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        Ok(())
    }

    fn postmaster_identity(&self) -> Result<PostmasterIdentity, LocalPostgresError> {
        let path = self.data.join("postmaster.pid");
        reject_link(&path)?;
        let bytes = std::fs::read(path).map_err(|_| LocalPostgresError::IdentityMismatch)?;
        if bytes.len() > 4096 {
            return Err(LocalPostgresError::IdentityMismatch);
        }
        let lines: Vec<_> = bytes
            .split(|byte| *byte == b'\n')
            .map(|line| line.trim_ascii())
            .collect();
        let mut text = lines.iter().map(|line| std::str::from_utf8(line).ok());
        let pid: u32 = lines
            .first()
            .and_then(|line| std::str::from_utf8(line).ok())
            .and_then(|line| line.parse().ok())
            .filter(|pid| *pid > 1)
            .ok_or(LocalPostgresError::IdentityMismatch)?;
        let directory = lines.get(1).ok_or(LocalPostgresError::IdentityMismatch)?;
        let started_seconds = text
            .nth(2)
            .flatten()
            .and_then(|line| line.parse().ok())
            .filter(|seconds| *seconds > 0)
            .ok_or(LocalPostgresError::IdentityMismatch)?;
        let port = text
            .next()
            .flatten()
            .and_then(|line| line.parse::<u16>().ok())
            .ok_or(LocalPostgresError::IdentityMismatch)?;
        if !self.postgres_path_matches(directory)? || port != self.manifest.port {
            return Err(LocalPostgresError::IdentityMismatch);
        }
        // postgres publishes ready only after successfully binding its port;
        // do not authenticate against a listener that won an earlier bind race.
        if lines.get(7).copied() != Some(b"ready".as_slice()) {
            return Err(LocalPostgresError::IdentityMismatch);
        }
        Ok(PostmasterIdentity {
            pid,
            started_seconds,
        })
    }

    /// Reapplies the ordinary runtime privilege boundary after migrations.
    /// Migration/retention/generation-control authority stays with the owner.
    pub async fn grant_runtime_access(&self) -> Result<(), LocalPostgresError> {
        let _verified = self.verified_connection().await?;
        let options =
            self.connection_options(OWNER_ROLE, &self.manifest.owner_password, APP_DATABASE);
        let mut connection =
            tokio::time::timeout(CONNECTION_TIMEOUT, PgConnection::connect_with(&options))
                .await
                .map_err(|_| LocalPostgresError::ProvisioningFailed)?
                .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        let mut transaction = connection
            .begin()
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        for statement in "SET LOCAL statement_timeout = '15s'; SET LOCAL lock_timeout = '5s'; GRANT orbisync_runtime TO orbisync; REVOKE CREATE ON SCHEMA public FROM PUBLIC, orbisync; GRANT USAGE ON SCHEMA public TO orbisync; REVOKE ALL ON ALL TABLES IN SCHEMA public FROM orbisync; GRANT SELECT ON ALL TABLES IN SCHEMA public TO orbisync; GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO orbisync; REVOKE ALL ON SCHEMA audit_operations FROM orbisync;".split(';') {
            if statement.trim().is_empty() { continue; }
            sqlx::query(statement).execute(&mut *transaction).await.map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        }
        let tables: Vec<String> = sqlx::query_scalar("SELECT tablename FROM pg_catalog.pg_tables WHERE schemaname = 'public' ORDER BY tablename")
            .fetch_all(&mut *transaction).await.map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        for table in tables {
            if table.starts_with("checkpoint_")
                || matches!(
                    table.as_str(),
                    "_sqlx_migrations" | "audit_events" | "audit_source_ips"
                )
            {
                continue;
            }
            let quoted = table.replace('"', "\"\"");
            sqlx::query(&format!(
                "GRANT INSERT, UPDATE, DELETE ON TABLE public.\"{quoted}\" TO orbisync"
            ))
            .execute(&mut *transaction)
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        }
        for statement in "GRANT SELECT, INSERT ON public.audit_events TO orbisync; REVOKE UPDATE, DELETE, TRUNCATE, REFERENCES, TRIGGER ON public.audit_events FROM orbisync; GRANT SELECT, INSERT, DELETE ON public.audit_source_ips TO orbisync; REVOKE UPDATE, TRUNCATE, REFERENCES, TRIGGER ON public.audit_source_ips FROM orbisync;".split(';') {
            if statement.trim().is_empty() { continue; }
            sqlx::query(statement).execute(&mut *transaction).await.map_err(|_| LocalPostgresError::ProvisioningFailed)?;
        }
        transaction
            .commit()
            .await
            .map_err(|_| LocalPostgresError::ProvisioningFailed)
    }

    /// Explicit operator repair. This is the only API that changes a saved
    /// database port; callers must persist the returned engine URL afterward.
    pub async fn repair_port(&mut self) -> Result<String, LocalPostgresError> {
        let result = self.repair_port_inner().await;
        match &result {
            Ok(_) => self.stage = "ready",
            Err(error) => self.stage = error.code(),
        }
        result
    }

    async fn repair_port_inner(&mut self) -> Result<String, LocalPostgresError> {
        if self.running.is_some() {
            self.stop_inner().await?;
        } else if self.data.join("postmaster.pid").exists() {
            if let Ok(identity) = self.adopted_identity() {
                let _verified = self.verified_connection().await?;
                self.running = Some(Running {
                    child: None,
                    identity,
                });
                self.stop_inner().await?;
            } else {
                let mut status = self.command("pg_ctl");
                status.arg("status").arg("-D").arg(&self.data);
                if run_command(status, Duration::from_secs(5)).await?.code() != Some(3) {
                    return Err(LocalPostgresError::IdentityMismatch);
                }
            }
        }
        let previous_reservation =
            TcpListener::bind((Ipv4Addr::LOCALHOST, self.manifest.port)).ok();
        let reservation = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .map_err(|_| LocalPostgresError::PortInUse)?;
        let new_port = reservation
            .local_addr()
            .map_err(|_| LocalPostgresError::PortInUse)?
            .port();
        let previous_port = self.manifest.port;
        self.manifest.port = new_port;
        if let Err(error) = self.persist_manifest() {
            self.manifest.port = previous_port;
            return Err(error);
        }
        drop(reservation);
        drop(previous_reservation);
        let result = self.start_inner().await;
        if result.is_err() && self.running.is_some() {
            // Roll back only after the newly tracked server has been stopped
            // with the same OS/SQL checks used for an explicit stop. If that
            // fails, retain its new port and identity for a safe retry. A start
            // that never established an identity also retains the new port.
            self.stop_inner().await?;
            let new_port = self.manifest.port;
            self.manifest.port = previous_port;
            if let Err(error) = self.persist_manifest() {
                self.manifest.port = new_port;
                return Err(error);
            }
        }
        result
    }

    fn persist_manifest(&self) -> Result<(), LocalPostgresError> {
        replace(
            &self.root.join("managed.json"),
            &serde_json::to_vec(&self.manifest).map_err(|_| LocalPostgresError::PrivateStorage)?,
        )
    }

    /// Stops the verified cluster after the engine has drained and closed its
    /// pool. It never sends a signal to an arbitrary service or stale PID.
    pub async fn stop(&mut self) -> Result<(), LocalPostgresError> {
        let result = self.stop_inner().await;
        match result {
            Ok(()) => self.stage = "stopped",
            Err(error) => self.stage = error.code(),
        }
        result
    }

    async fn stop_inner(&mut self) -> Result<(), LocalPostgresError> {
        self.clear_exited_running()?;
        let Some(running) = self.running.as_mut() else {
            return Ok(());
        };
        let mut expected = running.identity.clone();
        let observed = self.postmaster_identity()?;
        if expected.started_seconds == 0 && expected.pid == observed.pid {
            expected = observed.clone();
        }
        if observed != expected {
            return Err(LocalPostgresError::IdentityMismatch);
        }
        if self
            .running
            .as_ref()
            .is_some_and(|running| running.child.is_none())
        {
            self.verify_adopted_process(&observed)?;
        }
        let _connection = self.verified_connection().await?;
        let mut command = self.command("pg_ctl");
        command
            .arg("stop")
            .arg("-D")
            .arg(&self.data)
            .args(["-m", "fast", "-w", "-t", "15"]);
        if !run_command(command, Duration::from_secs(20))
            .await?
            .success()
        {
            return Err(LocalPostgresError::StopFailed);
        }
        if let Some(mut child) = self.running.take().and_then(|running| running.child) {
            let _reaped = child.wait();
        }
        Ok(())
    }

    fn clear_exited_running(&mut self) -> Result<(), LocalPostgresError> {
        let Some(running) = self.running.as_mut() else {
            return Ok(());
        };
        let exited = if let Some(child) = running.child.as_mut() {
            child
                .try_wait()
                .map_err(|_| LocalPostgresError::IdentityMismatch)?
                .is_some()
        } else {
            // A missing PID file is insufficient: the process might still be
            // shutting down or its PID might belong to another process. Only
            // a successful OS query proving this recorded PID absent permits
            // forgetting an adopted server. Any live/reused PID stays fenced.
            recorded_process_absent(running.identity.pid)?
        };
        if exited {
            self.running = None;
        }
        Ok(())
    }

    fn command(&self, executable: &str) -> Command {
        runtime_command(&self.runtime, executable)
    }

    fn verify_adopted_process(
        &self,
        identity: &PostmasterIdentity,
    ) -> Result<(), LocalPostgresError> {
        let executable = canonical(
            &self
                .runtime
                .join("bin")
                .join(format!("postgres{}", std::env::consts::EXE_SUFFIX)),
        )?;
        #[cfg(target_os = "linux")]
        {
            let process = PathBuf::from(format!("/proc/{}", identity.pid));
            if canonical(&process.join("exe"))? != executable {
                return Err(LocalPostgresError::IdentityMismatch);
            }
            let bytes = std::fs::read(process.join("cmdline"))
                .map_err(|_| LocalPostgresError::IdentityMismatch)?;
            let arguments: Vec<_> = bytes
                .split(|byte| *byte == 0)
                .filter(|value| !value.is_empty())
                .collect();
            if arguments.len() != 3 || arguments[1] != b"-D" {
                return Err(LocalPostgresError::IdentityMismatch);
            }
            let directory = std::str::from_utf8(arguments[2])
                .map_err(|_| LocalPostgresError::IdentityMismatch)?;
            if canonical(Path::new(directory))? != canonical(&self.data)? {
                return Err(LocalPostgresError::IdentityMismatch);
            }
            Ok(())
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt as _;
            let output = Command::new(windows_powershell()?).args(["-NoProfile", "-NonInteractive", "-Command",
                r#"$ErrorActionPreference='Stop'; $p=Get-CimInstance Win32_Process -Filter ('ProcessId = '+[uint32]$env:ORBISYNC_PG_PID); if ($null -eq $p -or [System.IO.Path]::GetFullPath($p.ExecutablePath) -ne [System.IO.Path]::GetFullPath($env:ORBISYNC_PG_EXE)) { exit 1 }; $e=[regex]::Escape($env:ORBISYNC_PG_EXE.Replace('\','/')); $d=[regex]::Escape($env:ORBISYNC_PG_DATA.Replace('\','/')); $pattern='^(?:"'+$e+'"|'+$e+')\s+-D\s+(?:"'+$d+'"|'+$d+')$'; if ($p.CommandLine.Trim().Replace('\','/') -notmatch $pattern) { exit 1 }; exit 0"#])
                .env("ORBISYNC_PG_PID", identity.pid.to_string()).env("ORBISYNC_PG_EXE", executable).env("ORBISYNC_PG_DATA", &self.data)
                .env_remove("PSModulePath")
                .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x08000000).status()
                .map_err(|_| LocalPostgresError::IdentityMismatch)?;
            if !output.success() {
                return Err(LocalPostgresError::IdentityMismatch);
            }
            Ok(())
        }
        #[cfg(not(any(windows, target_os = "linux")))]
        {
            let _identity = identity;
            let _executable = executable;
            Err(LocalPostgresError::IdentityMismatch)
        }
    }

    fn adopted_identity(&self) -> Result<PostmasterIdentity, LocalPostgresError> {
        let identity = self.postmaster_identity()?;
        self.verify_adopted_process(&identity)?;
        Ok(identity)
    }

    fn postgres_path_matches(&self, bytes: &[u8]) -> Result<bool, LocalPostgresError> {
        if self
            .path_encodings
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .iter()
            .any(|known| known == bytes)
        {
            return Ok(true);
        }
        let utf8_matches = std::str::from_utf8(bytes)
            .ok()
            .and_then(|path| canonical(Path::new(path)).ok())
            .is_some_and(|path| canonical(&self.data).ok().as_ref() == Some(&path));
        #[cfg(windows)]
        let matches = if utf8_matches {
            true
        } else {
            use std::os::windows::process::CommandExt as _;
            // Windows PostgreSQL writes filesystem paths in the system code
            // page even when its databases and client connections use UTF-8.
            // Decode only this path, never arbitrary child output or secrets.
            Command::new(windows_powershell()?).args(["-NoProfile","-NonInteractive","-Command",
                r#"$ErrorActionPreference='Stop'; $p=[System.Text.Encoding]::Default.GetString([Convert]::FromBase64String($env:ORBISYNC_PG_PATH_BYTES)); if ([System.IO.Path]::GetFullPath($p) -ne [System.IO.Path]::GetFullPath($env:ORBISYNC_PG_EXPECTED_PATH)) { exit 1 }; exit 0"#])
                .env_remove("PSModulePath").env("ORBISYNC_PG_PATH_BYTES", base64::engine::general_purpose::STANDARD.encode(bytes))
                .env("ORBISYNC_PG_EXPECTED_PATH",&self.data).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).creation_flags(0x08000000)
                .status().map_err(|_|LocalPostgresError::IdentityMismatch)?.success()
        };
        #[cfg(not(windows))]
        let matches = utf8_matches;
        if matches {
            let mut known = self
                .path_encodings
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if known.len() < 2 {
                known.push(bytes.to_vec());
            }
        }
        Ok(matches)
    }
}

fn recorded_process_absent(pid: u32) -> Result<bool, LocalPostgresError> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        let status = Command::new(windows_powershell()?)
            .args(["-NoProfile", "-NonInteractive", "-Command",
                "$ErrorActionPreference='Stop'; $p=Get-CimInstance Win32_Process -Filter ('ProcessId = '+[uint32]$env:ORBISYNC_PG_PID); if ($null -eq $p) { exit 3 }; exit 0"])
            .env("ORBISYNC_PG_PID", pid.to_string())
            .env_remove("PSModulePath").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null())
            .creation_flags(0x08000000).status().map_err(|_| LocalPostgresError::IdentityMismatch)?;
        match status.code() {
            Some(3) => Ok(true),
            Some(0) => Ok(false),
            _ => Err(LocalPostgresError::IdentityMismatch),
        }
    }
    #[cfg(target_os = "linux")]
    {
        match std::fs::metadata(format!("/proc/{pid}")) {
            Ok(_) => Ok(false),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                // hidepid can make another user's live/reused PID look absent.
                // Treat an unknown or restricted proc mount as inconclusive.
                let mounts = std::fs::read_to_string("/proc/mounts")
                    .map_err(|_| LocalPostgresError::IdentityMismatch)?;
                if !mounts.lines().any(|line| {
                    let mut fields = line.split_whitespace();
                    let _source = fields.next();
                    fields.next() == Some("/proc")
                        && fields.next() == Some("proc")
                        && fields.next().is_some_and(|options| {
                            options.split(',').all(|option| {
                                !option.starts_with("hidepid=")
                                    || matches!(option, "hidepid=0" | "hidepid=off")
                            })
                        })
                }) {
                    return Err(LocalPostgresError::IdentityMismatch);
                }
                Ok(true)
            }
            Err(_) => Err(LocalPostgresError::IdentityMismatch),
        }
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    {
        let _pid = pid;
        Err(LocalPostgresError::IdentityMismatch)
    }
}

impl Drop for LocalPostgres {
    fn drop(&mut self) {
        let Some(mut running) = self.running.take() else {
            return;
        };
        let Some(mut child) = running.child.take() else {
            return;
        };
        if child.try_wait().ok().flatten().is_some() {
            return;
        }
        if self.postmaster_identity().ok().as_ref() == Some(&running.identity) {
            let mut stop = self.command("pg_ctl");
            stop.arg("stop")
                .arg("-D")
                .arg(&self.data)
                .args(["-m", "fast", "-w", "-t", "3"]);
            if let Ok(mut command) = stop.spawn() {
                let deadline = Instant::now() + Duration::from_secs(5);
                while command.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
                    std::thread::sleep(Duration::from_millis(50));
                }
                let _killed = command.kill();
                let _reaped = command.wait();
            }
        }
        // This is the owned Child handle, never a PID read from disk.
        let _killed = child.kill();
        let _reaped = child.wait();
    }
}

fn runtime_command(runtime: &Path, name: &str) -> Command {
    let mut command = Command::new(
        runtime
            .join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX)),
    );
    command
        .current_dir(runtime)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for (key, _) in std::env::vars_os() {
        let name = key.to_string_lossy();
        if name.starts_with("PG") || name.starts_with("ORBISYNC_") || name == "DATABASE_URL" {
            command.env_remove(key);
        }
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        // Console Ctrl+C must drain the engine before the controller stops DB.
        command
            .process_group(0)
            .env("LD_LIBRARY_PATH", runtime.join("lib"));
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        command.creation_flags(0x08000000);
    }
    command
}

#[cfg(windows)]
fn windows_powershell() -> Result<PathBuf, LocalPostgresError> {
    let root =
        PathBuf::from(std::env::var_os("SystemRoot").ok_or(LocalPostgresError::PrivateStorage)?);
    if !root.is_absolute() {
        return Err(LocalPostgresError::PrivateStorage);
    }
    Ok(root.join("System32/WindowsPowerShell/v1.0/powershell.exe"))
}

fn validate_runtime(runtime: &Path) -> Result<(), LocalPostgresError> {
    for name in ["initdb", "pg_ctl", "postgres"] {
        let path = runtime
            .join("bin")
            .join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        if !path.is_file() || reject_link(&path).is_err() {
            return Err(LocalPostgresError::RuntimeMissing);
        }
    }
    let output = runtime_command(runtime, "postgres")
        .arg("--version")
        .stdout(Stdio::piped())
        .output()
        .map_err(|_| LocalPostgresError::RuntimeMissing)?;
    if !output.status.success() {
        return Err(LocalPostgresError::RuntimeMissing);
    }
    let version = String::from_utf8_lossy(&output.stdout);
    // postgres --version has a fixed non-secret output; all other child stdout
    // and stderr are discarded rather than forwarded into launcher/UI logs.
    if !version
        .split_whitespace()
        .any(|part| part.split('.').next() == Some("17"))
    {
        return Err(LocalPostgresError::VersionMismatch);
    }
    Ok(())
}

async fn run_command(
    mut command: Command,
    timeout: Duration,
) -> Result<ExitStatus, LocalPostgresError> {
    struct OwnedCommand(Child);
    impl Drop for OwnedCommand {
        fn drop(&mut self) {
            let _killed = self.0.kill();
            let _reaped = self.0.wait();
        }
    }
    let mut child = OwnedCommand(
        command
            .spawn()
            .map_err(|_| LocalPostgresError::StartFailed)?,
    );
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child
            .0
            .try_wait()
            .map_err(|_| LocalPostgresError::StartFailed)?
        {
            return Ok(status);
        }
        if Instant::now() >= deadline {
            return Err(LocalPostgresError::ReadinessTimeout);
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn canonical(path: &Path) -> Result<PathBuf, LocalPostgresError> {
    let path = std::fs::canonicalize(path).map_err(|_| LocalPostgresError::PrivateStorage)?;
    #[cfg(windows)]
    if let Some(path) = path.to_str().and_then(|path| path.strip_prefix(r"\\?\")) {
        return Ok(PathBuf::from(path));
    }
    Ok(path)
}

fn reject_link(path: &Path) -> Result<(), LocalPostgresError> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(LocalPostgresError::PrivateStorage);
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt as _;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err(LocalPostgresError::PrivateStorage);
                }
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(_) => Err(LocalPostgresError::PrivateStorage),
    }
}

fn private_options(options: &mut OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    #[cfg(windows)]
    let _options = options;
}

fn write_new(path: &Path, bytes: &[u8]) -> Result<(), LocalPostgresError> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    private_options(&mut options);
    let mut file = options
        .open(path)
        .map_err(|_| LocalPostgresError::PrivateStorage)?;
    file.write_all(bytes)
        .and_then(|()| file.sync_all())
        .map_err(|_| LocalPostgresError::PrivateStorage)
}

fn replace(path: &Path, bytes: &[u8]) -> Result<(), LocalPostgresError> {
    reject_link(path)?;
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::now_v7()));
    write_new(&temporary, bytes)?;
    if let Err(_error) = std::fs::rename(&temporary, path) {
        let _removed = std::fs::remove_file(temporary);
        return Err(LocalPostgresError::PrivateStorage);
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#[path = "local_postgres_tests.rs"]
mod tests;
