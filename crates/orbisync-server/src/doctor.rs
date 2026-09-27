//! Non-destructive startup diagnostics for the `doctor` CLI command.

use std::collections::BTreeMap;
use std::io::{self, Write as _};
use std::path::Path;

use orbisync_config::{Config, ConfigError, EnvSource};
use orbisync_identity::PasswordPolicy;
use orbisync_storage_postgres::{MIGRATOR, create_pool};

use super::{PASSWORD_DENYLIST_ENV, read_password_corpus};

const CHECK_NAMES: [&str; 7] = [
    "configuration",
    "secrets",
    "database",
    "migrations",
    "checkpoint_store",
    "password_denylist",
    "bind_address",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CheckStatus {
    Pass,
    Warning,
    Fail,
    Skipped,
}

impl CheckStatus {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "pass",
            Self::Warning => "warning",
            Self::Fail => "fail",
            Self::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DoctorCheck {
    name: &'static str,
    status: CheckStatus,
    detail: String,
}

impl DoctorCheck {
    fn pass(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Pass,
            detail: detail.into(),
        }
    }

    fn warning(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Warning,
            detail: detail.into(),
        }
    }

    fn fail(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Fail,
            detail: detail.into(),
        }
    }

    fn skipped(name: &'static str, detail: impl Into<String>) -> Self {
        Self {
            name,
            status: CheckStatus::Skipped,
            detail: detail.into(),
        }
    }
}

/// Complete, secret-free diagnostic result.
#[derive(Debug)]
pub(crate) struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    /// Produces a report when configuration discovery/loading failed before
    /// dependent checks could run.
    pub(crate) fn configuration_failure(error: &ConfigError) -> Self {
        let mut checks = vec![DoctorCheck::fail(
            "configuration",
            format!("{}: {}", error.key(), error.detail()),
        )];
        checks.extend(
            CHECK_NAMES
                .iter()
                .skip(1)
                .map(|name| DoctorCheck::skipped(name, "configuration is invalid")),
        );
        Self { checks }
    }

    /// Returns true when no required diagnostic failed.
    pub(crate) fn is_healthy(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.status != CheckStatus::Fail)
    }

    /// Writes either stable JSON or a compact human-readable report.
    pub(crate) fn emit(&self, json: bool) -> io::Result<()> {
        let rendered = if json {
            serde_json::to_string_pretty(&self.as_json()).map_err(io::Error::other)?
        } else {
            self.as_text()
        };
        let mut stdout = io::stdout().lock();
        stdout.write_all(rendered.as_bytes())?;
        stdout.write_all(b"\n")?;
        stdout.flush()
    }

    fn as_json(&self) -> serde_json::Value {
        serde_json::json!({
            "service": "orbisync",
            "version": super::VERSION,
            "ok": self.is_healthy(),
            "checks": self.checks.iter().map(|check| serde_json::json!({
                "name": check.name,
                "status": check.status.as_str(),
                "detail": check.detail,
            })).collect::<Vec<_>>(),
        })
    }

    fn as_text(&self) -> String {
        let mut lines = Vec::with_capacity(self.checks.len() + 1);
        lines.push(format!(
            "OrbiSync doctor: {}",
            if self.is_healthy() { "PASS" } else { "FAIL" }
        ));
        lines.extend(self.checks.iter().map(|check| {
            format!(
                "[{}] {} - {}",
                check.status.as_str().to_ascii_uppercase(),
                check.name,
                check.detail
            )
        }));
        lines.join("\n")
    }
}

/// Runs every non-destructive startup diagnostic whose configuration loaded.
pub(crate) async fn evaluate(
    config: &Config,
    env: &dyn EnvSource,
    warnings: &[String],
    config_source: &str,
) -> DoctorReport {
    let configuration = if warnings.is_empty() {
        DoctorCheck::pass(
            "configuration",
            format!("valid configuration loaded from {config_source}"),
        )
    } else {
        DoctorCheck::warning(
            "configuration",
            format!(
                "valid configuration loaded from {config_source}; {} unknown key warning(s)",
                warnings.len()
            ),
        )
    };
    let secrets = check_secrets(config, env);

    let database_url = env
        .get(&config.database.url_env)
        .filter(|value| !value.trim().is_empty());
    let (database, migrations, checkpoint_store) = if let Some(database_url) = database_url {
        check_database(config, &database_url).await
    } else {
        (
            DoctorCheck::skipped("database", "database URL secret is not set"),
            DoctorCheck::skipped("migrations", "database connection was not attempted"),
            DoctorCheck::skipped("checkpoint_store", "database connection was not attempted"),
        )
    };

    let password_denylist = check_password_denylist(config, env);
    let bind_address = check_bind_address(config).await;

    DoctorReport {
        checks: vec![
            configuration,
            secrets,
            database,
            migrations,
            checkpoint_store,
            password_denylist,
            bind_address,
        ],
    }
}

fn check_secrets(config: &Config, env: &dyn EnvSource) -> DoctorCheck {
    let mut missing = config
        .required_secret_env_vars()
        .into_iter()
        .filter(|name| env.get(name).is_none_or(|value| value.trim().is_empty()))
        .map(str::to_owned)
        .collect::<Vec<_>>();
    missing.sort();
    missing.dedup();
    if missing.is_empty() {
        DoctorCheck::pass(
            "secrets",
            format!(
                "{} required secret environment variable(s) are set",
                config.required_secret_env_vars().len()
            ),
        )
    } else {
        DoctorCheck::fail(
            "secrets",
            format!("missing environment variable(s): {}", missing.join(", ")),
        )
    }
}

async fn check_database(
    config: &Config,
    database_url: &str,
) -> (DoctorCheck, DoctorCheck, DoctorCheck) {
    let pool = match create_pool(&config.database, database_url) {
        Ok(pool) => pool,
        Err(_) => {
            return (
                DoctorCheck::fail("database", "database URL cannot create a PostgreSQL pool"),
                DoctorCheck::skipped("migrations", "database pool creation failed"),
                DoctorCheck::skipped("checkpoint_store", "database pool creation failed"),
            );
        }
    };

    if sqlx::query_scalar::<_, i32>("SELECT 1")
        .fetch_one(&pool)
        .await
        .is_err()
    {
        pool.close().await;
        return (
            DoctorCheck::fail(
                "database",
                "PostgreSQL did not answer the connectivity query",
            ),
            DoctorCheck::skipped("migrations", "database connectivity failed"),
            DoctorCheck::skipped("checkpoint_store", "database connectivity failed"),
        );
    }

    let database = DoctorCheck::pass("database", "PostgreSQL connectivity query succeeded");
    let migrations = match sqlx::query_as::<_, (i64, bool, Vec<u8>)>(
        "SELECT version, success, checksum FROM _sqlx_migrations ORDER BY version",
    )
    .fetch_all(&pool)
    .await
    {
        Ok(rows) => assess_migrations(&rows),
        Err(_) => DoctorCheck::fail(
            "migrations",
            "migration metadata is missing or cannot be read",
        ),
    };
    let checkpoint_store = if sqlx::query(
        "SELECT instance_id, revision, data, created_at FROM instance_checkpoints LIMIT 0",
    )
    .fetch_all(&pool)
    .await
    .is_ok()
    {
        DoctorCheck::pass(
            "checkpoint_store",
            "instance_checkpoints schema is present and readable",
        )
    } else {
        DoctorCheck::fail(
            "checkpoint_store",
            "instance_checkpoints schema is missing or cannot be read",
        )
    };
    pool.close().await;
    (database, migrations, checkpoint_store)
}

fn assess_migrations(rows: &[(i64, bool, Vec<u8>)]) -> DoctorCheck {
    let expected = MIGRATOR
        .iter()
        .map(|migration| (migration.version, migration.checksum.as_ref().to_vec()))
        .collect::<BTreeMap<_, _>>();
    let actual = rows
        .iter()
        .map(|(version, success, checksum)| (*version, (*success, checksum)))
        .collect::<BTreeMap<_, _>>();

    let pending = expected
        .keys()
        .filter(|version| !actual.contains_key(version))
        .copied()
        .collect::<Vec<_>>();
    let unexpected = actual
        .keys()
        .filter(|version| !expected.contains_key(version))
        .copied()
        .collect::<Vec<_>>();
    let failed = actual
        .iter()
        .filter_map(|(version, (success, _))| (!*success).then_some(*version))
        .collect::<Vec<_>>();
    let checksum_mismatch = expected
        .iter()
        .filter_map(|(version, checksum)| {
            actual
                .get(version)
                .filter(|(_, actual_checksum)| actual_checksum.as_slice() != checksum.as_slice())
                .map(|_| *version)
        })
        .collect::<Vec<_>>();

    if pending.is_empty()
        && unexpected.is_empty()
        && failed.is_empty()
        && checksum_mismatch.is_empty()
    {
        return DoctorCheck::pass(
            "migrations",
            format!("all {} embedded migration(s) are applied", expected.len()),
        );
    }

    let mut problems = Vec::new();
    if !pending.is_empty() {
        problems.push(format!("pending versions: {}", join_versions(&pending)));
    }
    if !unexpected.is_empty() {
        problems.push(format!(
            "unknown applied versions: {}",
            join_versions(&unexpected)
        ));
    }
    if !failed.is_empty() {
        problems.push(format!("failed versions: {}", join_versions(&failed)));
    }
    if !checksum_mismatch.is_empty() {
        problems.push(format!(
            "checksum mismatch: {}",
            join_versions(&checksum_mismatch)
        ));
    }
    DoctorCheck::fail("migrations", problems.join("; "))
}

fn join_versions(versions: &[i64]) -> String {
    versions
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(", ")
}

fn check_password_denylist(config: &Config, env: &dyn EnvSource) -> DoctorCheck {
    let Some(path) = env
        .get(PASSWORD_DENYLIST_ENV)
        .filter(|value| !value.trim().is_empty())
    else {
        return DoctorCheck::fail(
            "password_denylist",
            format!("{PASSWORD_DENYLIST_ENV} is not set"),
        );
    };
    let entries = match read_password_corpus(Path::new(&path)) {
        Ok(entries) => entries,
        Err(_) => {
            return DoctorCheck::fail(
                "password_denylist",
                "denylist file is missing, unreadable, or not UTF-8",
            );
        }
    };
    match PasswordPolicy::production(entries)
        .map(|policy| policy.with_min_length(config.auth.password_min_length))
    {
        Ok(_) => DoctorCheck::pass(
            "password_denylist",
            "approved 10,000-entry password corpus is readable",
        ),
        Err(_) => DoctorCheck::fail(
            "password_denylist",
            "denylist must contain exactly 10,000 distinct approved entries",
        ),
    }
}

async fn check_bind_address(config: &Config) -> DoctorCheck {
    match tokio::net::TcpListener::bind(&config.server.bind).await {
        Ok(listener) => {
            drop(listener);
            DoctorCheck::pass(
                "bind_address",
                format!("{} is currently available", config.server.bind),
            )
        }
        Err(error) => DoctorCheck::fail(
            "bind_address",
            format!("{} is unavailable ({:?})", config.server.bind, error.kind()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use orbisync_config::MapEnv;
    use std::collections::BTreeSet;

    #[test]
    fn secret_check_lists_names_but_never_values() {
        let config = Config::default();
        let secret_value = "never-print-this-secret";
        let env = MapEnv::from_pairs([
            (config.database.url_env.clone(), secret_value),
            (config.auth.token_signing_key_env.clone(), secret_value),
        ]);
        let check = check_secrets(&config, &env);
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(!check.detail.contains(secret_value));
        assert!(check.detail.contains(&config.auth.pagination_hmac_key_env));
    }

    #[test]
    fn configuration_failure_has_stable_complete_shape() {
        let error = ConfigError::new(
            orbisync_config::ConfigErrorKind::InvalidValue,
            "server.bind",
            "invalid bind address",
        );
        let report = DoctorReport::configuration_failure(&error);
        assert!(!report.is_healthy());
        assert_eq!(report.checks.len(), CHECK_NAMES.len());
        assert_eq!(report.checks[0].status, CheckStatus::Fail);
        assert!(
            report
                .checks
                .iter()
                .skip(1)
                .all(|check| check.status == CheckStatus::Skipped)
        );
    }

    #[test]
    fn migration_assessment_rejects_missing_and_changed_versions() {
        let mut rows = MIGRATOR
            .iter()
            .map(|migration| {
                (
                    migration.version,
                    true,
                    migration.checksum.as_ref().to_vec(),
                )
            })
            .collect::<Vec<_>>();
        let complete = assess_migrations(&rows);
        assert_eq!(complete.status, CheckStatus::Pass);

        let removed = rows.pop();
        assert!(removed.is_some());
        let missing = assess_migrations(&rows);
        assert_eq!(missing.status, CheckStatus::Fail);
        assert!(missing.detail.contains("pending versions"));

        if let Some((_, _, checksum)) = rows.first_mut() {
            checksum.push(0);
        }
        let changed = assess_migrations(&rows);
        assert_eq!(changed.status, CheckStatus::Fail);
        assert!(changed.detail.contains("checksum mismatch"));
    }

    #[test]
    fn json_report_has_machine_readable_statuses() {
        let report = DoctorReport {
            checks: vec![
                DoctorCheck::pass("configuration", "valid"),
                DoctorCheck::warning("secrets", "warning"),
            ],
        };
        let value = report.as_json();
        assert_eq!(value["ok"], true);
        assert_eq!(value["checks"][0]["status"], "pass");
        assert_eq!(value["checks"][1]["status"], "warning");
    }

    #[test]
    fn migration_versions_are_unique() {
        let versions = MIGRATOR
            .iter()
            .map(|migration| migration.version)
            .collect::<BTreeSet<_>>();
        assert_eq!(versions.len(), MIGRATOR.iter().count());
    }
}
