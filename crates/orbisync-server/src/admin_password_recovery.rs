//! Local operator recovery: no HTTP listener, migrations, or bootstrap.
use super::*;

pub(super) async fn run(
    config: &Config,
    env: &impl EnvSource,
    login: &str,
    denylist: &Path,
    output_path: &Path,
) -> Result<(), ServerError> {
    let login = LoginId::new(login).map_err(|error| ServerError::Recovery(error.into()))?;
    let policy = PasswordPolicy::production(read_password_corpus(denylist)?)
        .map(|p| p.with_min_length(config.auth.password_min_length))
        .map_err(|_| ServerError::PasswordPolicy)?;
    let passwords = PasswordService::new_with_argon2_and_concurrency(
        policy,
        config.auth.argon2_memory_cost_kib,
        config.auth.argon2_iterations,
        config.auth.argon2_parallelism,
        config.auth.password_hash_concurrency as usize,
    )
    .map_err(|_| ServerError::PasswordPolicy)?;
    // Validate/protect the parent before creating even an empty credential file.
    // Reuse the launcher's Windows ACL and Unix mode checks.
    let parent = output_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    web_admin::private_dir(parent).map_err(ServerError::RecoveryIo)?;
    let mut output = PasswordOutput::prepare(Some(output_path)).map_err(|error| match error {
        ServerError::BootstrapIo(error) => ServerError::RecoveryIo(error),
        other => other,
    })?;
    let database_url = env
        .get(&config.database.url_env)
        .filter(|url| !url.is_empty())
        .ok_or_else(|| {
            ServerError::Recovery(orbisync_application::ApplicationError::port_failure(
                "configured database URL is required",
            ))
        })?;
    let pool = create_pool(&config.database, &database_url)?;
    let repository = PgIdentityRepository::new(pool.clone());
    let service = IdentityAdministrationService::new(
        Arc::new(IdentityAdministrationStore::new(pool)),
        Arc::new(SystemClock::new()),
        passwords,
    );
    let request_id = orbisync_application::RequestId::new(format!("req_{}", UserId::generate()))
        .map_err(|error| ServerError::Recovery(error.into()))?;
    let temporary = service
        .recover_local_administrator(&repository, &login, request_id)
        .await
        .map_err(ServerError::Recovery)?;
    // A disk failure after commit cannot roll back the database. Report it explicitly;
    // the operator can recover again to a fresh file, invalidating this credential.
    output.write(temporary.expose_secret()).map_err(|_| {
        ServerError::Recovery(orbisync_application::ApplicationError::port_failure(
            "password reset committed but credential output failed; run recovery again with a new private output file",
        ))
    })?;
    eprintln!(
        "Administrator password recovered. Read the specified private file, sign in, and change the temporary password. Previous sessions are revoked."
    );
    Ok(())
}
