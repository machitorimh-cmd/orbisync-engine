use super::*;

struct Fixture {
    root: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let root =
            std::env::temp_dir().join(format!("orbisync-managed-pg-{}", uuid::Uuid::now_v7()));
        crate::web_admin::private_dir(&root).unwrap();
        Self { root }
    }
    fn controller(&self) -> LocalPostgres {
        LocalPostgres::open_storage(&self.root, &self.root).unwrap()
    }
    fn fake_stopped_cluster(&self, controller: &LocalPostgres) {
        crate::web_admin::private_dir(&controller.data).unwrap();
        crate::web_admin::private_dir(&controller.data.join("global")).unwrap();
        write_new(&controller.data.join("PG_VERSION"), b"17\n").unwrap();
        write_new(
            &controller.data.join("global/pg_control"),
            b"test preflight only; never launched",
        )
        .unwrap();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        // Delete only this newly created disposable test directory.
        if let (Ok(root), Ok(temporary)) = (canonical(&self.root), canonical(&std::env::temp_dir()))
            && root.starts_with(temporary)
            && root
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("orbisync-managed-pg-"))
        {
            let _removed = std::fs::remove_dir_all(root);
        }
    }
}

#[test]
fn managed_postgres_private_manifest_stable_credentials_and_exclusive_lock() {
    let fixture = Fixture::new();
    let controller = fixture.controller();
    assert!(LocalPostgres::managed(&fixture.root));
    assert_eq!(
        LocalPostgres::open_storage(&fixture.root, &fixture.root).unwrap_err(),
        LocalPostgresError::InUse
    );
    let owner = controller.manifest.owner_password.clone();
    let app = controller.manifest.application_password.clone();
    let url = controller.manifest.application_url();
    let id = controller.manifest.installation_id;
    for output in [
        serde_json::to_string(&controller.status()).unwrap(),
        format!("{controller:?}"),
    ] {
        assert!(!output.contains(&owner));
        assert!(!output.contains(&app));
        assert!(!output.contains("postgres://"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(
            std::fs::metadata(controller.root.join("managed.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
    drop(controller);
    let reopened = fixture.controller();
    assert_eq!(reopened.manifest.owner_password, owner);
    assert_eq!(reopened.manifest.application_password, app);
    assert_eq!(reopened.manifest.application_url(), url);
    assert_eq!(reopened.manifest.installation_id, id);
}

#[test]
fn managed_postgres_preserves_unowned_and_corrupt_installations() {
    let fixture = Fixture::new();
    let private = fixture.root.join("local-postgres");
    crate::web_admin::private_dir(&private).unwrap();
    // allow-hardcoded-secret: fixture directory name, never credential data.
    let important = private.join("existing-database");
    write_new(&important, b"preserve this existing data").unwrap();
    assert_eq!(
        LocalPostgres::open_storage(&fixture.root, &fixture.root).unwrap_err(),
        LocalPostgresError::RecoveryRequired
    );
    assert_eq!(
        std::fs::read(&important).unwrap(),
        b"preserve this existing data"
    );
    assert!(!LocalPostgres::managed(&fixture.root));
    let separate = Fixture::new();
    let controller = separate.controller();
    let manifest = controller.root.join("managed.json");
    drop(controller);
    std::fs::write(&manifest, b"{broken private manifest").unwrap();
    assert_eq!(
        LocalPostgres::open_storage(&separate.root, &separate.root).unwrap_err(),
        LocalPostgresError::RecoveryRequired
    );
    assert_eq!(
        std::fs::read(&manifest).unwrap(),
        b"{broken private manifest"
    );
}

#[tokio::test]
async fn managed_postgres_incomplete_initialization_is_preserved_for_recovery() {
    let fixture = Fixture::new();
    let mut controller = fixture.controller();
    crate::web_admin::private_dir(&controller.data).unwrap();
    let important = controller.data.join("partial-database");
    write_new(&important, b"do not recreate or delete").unwrap();
    assert_eq!(
        controller.start().await.unwrap_err(),
        LocalPostgresError::RecoveryRequired
    );
    assert_eq!(controller.status().state, "recovery_required");
    assert_eq!(
        std::fs::read(important).unwrap(),
        b"do not recreate or delete"
    );
    controller.stop().await.unwrap();
}

#[tokio::test]
async fn managed_postgres_port_conflict_never_changes_url_or_stops_foreign_listener() {
    let fixture = Fixture::new();
    let mut controller = fixture.controller();
    fixture.fake_stopped_cluster(&controller);
    let original_url = controller.manifest.application_url();
    let foreign = TcpListener::bind((Ipv4Addr::LOCALHOST, controller.manifest.port)).unwrap();
    assert_eq!(
        controller.start().await.unwrap_err(),
        LocalPostgresError::PortInUse
    );
    assert_eq!(controller.status().state, "port_in_use");
    assert_eq!(controller.manifest.application_url(), original_url);
    controller.stop().await.unwrap();
    assert_eq!(
        foreign.local_addr().unwrap().port(),
        controller.manifest.port
    );
    assert!(TcpListener::bind(foreign.local_addr().unwrap()).is_err());
}

#[test]
fn managed_postgres_operations_are_send_for_owned_http_lifecycle() {
    fn send(_: impl Send) {}
    let fixture = Fixture::new();
    let mut controller = fixture.controller();
    send(controller.start());
    send(controller.stop());
    send(controller.repair_port());
    send(controller.grant_runtime_access());
}

#[test]
#[cfg(windows)]
fn managed_postgres_windows_filesystem_codepage_identity_matches_unicode_path() {
    use std::os::windows::process::CommandExt as _;
    let fixture = Fixture::new();
    let install = fixture.root.join("private 日本語 path");
    let controller = LocalPostgres::open_storage(&install, &fixture.root).unwrap();
    fixture.fake_stopped_cluster(&controller);
    let output = Command::new("powershell.exe").args(["-NoProfile", "-NonInteractive", "-Command",
        "$p=$env:ORBISYNC_PG_EXPECTED_PATH.Replace('\\','/'); [Console]::WriteLine([Convert]::ToBase64String([System.Text.Encoding]::Default.GetBytes($p)))"])
        .env_remove("PSModulePath").env("ORBISYNC_PG_EXPECTED_PATH", &controller.data).creation_flags(0x08000000).output().unwrap();
    assert!(output.status.success());
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(String::from_utf8(output.stdout).unwrap().trim())
        .unwrap();
    assert!(controller.postgres_path_matches(&bytes).unwrap());
}

#[tokio::test]
async fn managed_postgres_rejects_auto_configuration_overrides_without_altering_them() {
    let fixture = Fixture::new();
    let mut controller = fixture.controller();
    fixture.fake_stopped_cluster(&controller);
    let automatic = controller.data.join("postgresql.auto.conf");
    let original = b"# Explicit user modification\nlisten_addresses = '*'\n";
    write_new(&automatic, original).unwrap();
    assert_eq!(
        controller.start().await.unwrap_err(),
        LocalPostgresError::RecoveryRequired
    );
    assert_eq!(std::fs::read(automatic).unwrap(), original);
    assert!(controller.running.is_none());
}

#[test]
fn managed_postgres_requires_ready_postmaster_identity_before_authenticating() {
    let fixture = Fixture::new();
    let controller = fixture.controller();
    fixture.fake_stopped_cluster(&controller);
    let contents = |state: &str| {
        format!(
            "{}\n{}\n1700000000\n{}\n\n127.0.0.1\n0\n{state}\n",
            std::process::id(),
            controller.data.display(),
            controller.manifest.port
        )
    };
    let pidfile = controller.data.join("postmaster.pid");
    write_new(&pidfile, contents("starting").as_bytes()).unwrap();
    assert_eq!(
        controller.postmaster_identity(),
        Err(LocalPostgresError::IdentityMismatch)
    );
    replace(&pidfile, contents("ready   ").as_bytes()).unwrap();
    assert_eq!(
        controller.postmaster_identity().unwrap().pid,
        std::process::id()
    );
}

#[test]
#[cfg(windows)]
fn managed_postgres_windows_acl_validation_preserves_rejection_of_shared_directories() {
    use std::os::windows::process::CommandExt as _;
    let fixture = Fixture::new();
    let shared = fixture.root.join("shared test directory");
    std::fs::create_dir(&shared).unwrap();
    assert!(
        Command::new("icacls")
            .arg(&shared)
            .args(["/grant", "*S-1-1-0:(OI)(CI)R"])
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .status
            .success()
    );
    let acl = || {
        Command::new("icacls")
            .arg(&shared)
            .creation_flags(0x08000000)
            .output()
            .unwrap()
            .stdout
    };
    let before = acl();
    assert!(
        crate::web_admin::private_dir(&shared).is_err(),
        "Everyone read access must remain forbidden"
    );
    assert_eq!(
        acl(),
        before,
        "validation must never rewrite a pre-existing directory ACL"
    );
}

#[tokio::test]
async fn managed_postgres_real_runtime_lifecycle_permissions_and_explicit_repair() {
    let Some(runtime) = std::env::var_os("ORBISYNC_TEST_POSTGRES_RUNTIME") else {
        eprintln!(
            "managed PostgreSQL real-runtime test: set ORBISYNC_TEST_POSTGRES_RUNTIME to the bundled runtime"
        );
        return;
    };
    let fixture = Fixture::new();
    let install = fixture.root.join("private cluster 日本語 with spaces");
    let mut controller = LocalPostgres::open_with_runtime(&install, Path::new(&runtime)).unwrap();
    let url = controller.start().await.unwrap();
    assert_eq!(controller.status().state, "ready");
    // allow-hardcoded-secret: asserts removal of a private temporary filename, never a password value.
    assert!(!controller.root.join("initdb-password.txt").exists());
    let migration = sqlx::PgPool::connect(&controller.migration_database_url())
        .await
        .unwrap();
    orbisync_storage_postgres::run_migrations(&migration)
        .await
        .unwrap();
    sqlx::query("CREATE TABLE managed_runtime_probe(value TEXT NOT NULL)")
        .execute(&migration)
        .await
        .unwrap();
    controller.grant_runtime_access().await.unwrap();
    let app = sqlx::PgPool::connect(&url).await.unwrap();
    let privileges: (bool,bool,bool,bool,bool,bool,bool) = sqlx::query_as("SELECT rolsuper, rolcreaterole, rolcreatedb, has_table_privilege(current_user, 'public.audit_events', 'DELETE'), has_table_privilege(current_user, 'public.checkpoint_writer_control', 'UPDATE'), has_table_privilege(current_user, 'public.users', 'INSERT'), has_schema_privilege(current_user, 'audit_operations', 'USAGE') FROM pg_roles WHERE rolname = current_user")
        .fetch_one(&app).await.unwrap();
    assert_eq!(privileges, (false, false, false, false, false, true, false));
    assert!(
        sqlx::query("CREATE TABLE should_be_forbidden(id INT)")
            .execute(&app)
            .await
            .is_err()
    );
    sqlx::query("INSERT INTO managed_runtime_probe(value) VALUES ('persist across restarts')")
        .execute(&app)
        .await
        .unwrap();
    app.close().await;
    migration.close().await;
    controller.stop().await.unwrap();
    assert_eq!(controller.status().state, "stopped");
    drop(controller);
    let mut controller = LocalPostgres::open_with_runtime(&install, Path::new(&runtime)).unwrap();
    assert_eq!(controller.start().await.unwrap(), url);
    let app = sqlx::PgPool::connect(&url).await.unwrap();
    let restored: String = sqlx::query_scalar("SELECT value FROM managed_runtime_probe")
        .fetch_one(&app)
        .await
        .unwrap();
    assert_eq!(restored, "persist across restarts");
    app.close().await;
    // Simulate the launcher disappearing while PostgreSQL remains alive, but
    // retain the actual Child handle in a test guard so even a failed assertion
    // cannot leave a test process unowned.
    struct Detached(Child);
    impl Drop for Detached {
        fn drop(&mut self) {
            let _killed = self.0.kill();
            let _reaped = self.0.wait();
        }
    }
    let _detached = Detached(controller.running.as_mut().unwrap().child.take().unwrap());
    drop(controller);
    let mut controller = LocalPostgres::open_with_runtime(&install, Path::new(&runtime)).unwrap();
    assert_eq!(controller.start().await.unwrap(), url);
    assert!(
        controller.running.as_ref().unwrap().child.is_none(),
        "surviving managed process is reused only after OS and SQL identity checks"
    );
    controller.stop().await.unwrap();
    let old_port = controller.manifest.port;
    let foreign = TcpListener::bind((Ipv4Addr::LOCALHOST, old_port)).unwrap();
    assert_eq!(
        controller.start().await.unwrap_err(),
        LocalPostgresError::PortInUse
    );
    let repaired_url = controller.repair_port().await.unwrap();
    assert_ne!(controller.manifest.port, old_port);
    assert_ne!(repaired_url, url);
    assert_eq!(foreign.local_addr().unwrap().port(), old_port);
    let app = sqlx::PgPool::connect(&repaired_url).await.unwrap();
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT value FROM managed_runtime_probe")
            .fetch_one(&app)
            .await
            .unwrap(),
        restored
    );
    app.close().await;
    controller.stop().await.unwrap();
}
