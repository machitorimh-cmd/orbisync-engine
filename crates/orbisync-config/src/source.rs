//! Configuration sources and their precedence.
//!
//! Precedence, lowest first: safe defaults, configuration file, environment
//! variables, CLI arguments (specification §28.3). Each layer applies through
//! [`Config::apply`], so all layers share one schema and one set of type
//! checks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::error::{ConfigError, ConfigErrorKind};
use crate::keys::{CONFIG_KEYS, ENV_PREFIX, env_var_for_key};
use crate::model::Config;

/// `ORBISYNC_*` variables that are consumed outside the configuration schema.
///
/// They are read by the binary itself, so they are not unknown keys.
const RESERVED_ENV_VARS: &[&str] = &["ORBISYNC_CONFIG_FILE", "ORBISYNC_PASSWORD_DENYLIST_FILE"];

/// Read only view of the process environment.
///
/// The trait keeps the loader testable: unit tests inject a [`MapEnv`] instead
/// of mutating the real environment.
pub trait EnvSource {
    /// Returns the value of `name`, if it is set.
    fn get(&self, name: &str) -> Option<String>;

    /// Returns every variable name that starts with `prefix`.
    fn names_with_prefix(&self, prefix: &str) -> Vec<String>;
}

/// [`EnvSource`] backed by the real process environment.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl SystemEnv {
    /// Creates a system environment source.
    #[must_use]
    pub const fn new() -> Self {
        Self
    }
}

impl EnvSource for SystemEnv {
    fn get(&self, name: &str) -> Option<String> {
        std::env::var(name).ok()
    }

    fn names_with_prefix(&self, prefix: &str) -> Vec<String> {
        std::env::vars()
            .map(|(name, _)| name)
            .filter(|name| name.starts_with(prefix))
            .collect()
    }
}

/// In-memory [`EnvSource`] for tests and for deterministic bootstrapping.
#[derive(Debug, Clone, Default)]
pub struct MapEnv {
    values: BTreeMap<String, String>,
}

impl MapEnv {
    /// Builds an environment from name and value pairs.
    #[must_use]
    pub fn from_pairs<I, K, V>(pairs: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        Self {
            values: pairs
                .into_iter()
                .map(|(name, value)| (name.into(), value.into()))
                .collect(),
        }
    }
}

impl EnvSource for MapEnv {
    fn get(&self, name: &str) -> Option<String> {
        self.values.get(name).cloned()
    }

    fn names_with_prefix(&self, prefix: &str) -> Vec<String> {
        self.values
            .keys()
            .filter(|name| name.starts_with(prefix))
            .cloned()
            .collect()
    }
}

/// How the loader reacts to a key that is not part of the schema.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum UnknownKeyPolicy {
    /// Collect the key as a warning and continue (default, §6.2).
    #[default]
    Warn,
    /// Abort the load.
    Reject,
}

/// A validated configuration together with the warnings raised while loading.
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    /// The validated configuration.
    pub config: Config,
    /// Human readable warnings, for example unknown keys.
    pub warnings: Vec<String>,
}

impl Config {
    /// Resolves the OC-05 configuration search order.
    ///
    /// An explicit CLI or environment path is authoritative, including when
    /// it does not exist; the caller then receives a startup read error rather
    /// than silently falling back to another file.
    pub fn discover_path(
        cli_path: Option<&Path>,
        env: &dyn EnvSource,
    ) -> Result<Option<PathBuf>, ConfigError> {
        if let Some(path) = cli_path {
            return Ok(Some(path.to_owned()));
        }
        if let Some(path) = env.get("ORBISYNC_CONFIG_FILE") {
            return Ok(Some(PathBuf::from(path)));
        }
        let local = std::env::current_dir()
            .map_err(|error| {
                ConfigError::new(ConfigErrorKind::FileUnreadable, ".", error.to_string())
            })?
            .join("orbisync.toml");
        if local.is_file() {
            return Ok(Some(local));
        }
        let system = PathBuf::from("/etc/orbisync/orbisync.toml");
        if system.is_file() {
            return Ok(Some(system));
        }
        Ok(None)
    }

    /// Loads the configuration from every source and validates the result.
    ///
    /// `cli_overrides` are dotted key and value pairs produced by the binary's
    /// argument parser; they win over every other source.
    ///
    /// # Errors
    ///
    /// Returns the first [`ConfigError`] raised while reading the file,
    /// applying a layer or validating the merged result.
    pub fn load(
        file: Option<&Path>,
        env: &dyn EnvSource,
        cli_overrides: &[(String, String)],
    ) -> Result<LoadedConfig, ConfigError> {
        Self::load_with_policy(file, env, cli_overrides, UnknownKeyPolicy::default())
    }

    /// Loads the configuration with an explicit unknown key policy.
    ///
    /// # Errors
    ///
    /// As [`Config::load`], and additionally returns
    /// [`ConfigErrorKind::UnknownKey`] when `policy` is
    /// [`UnknownKeyPolicy::Reject`] and an unknown key is present.
    pub fn load_with_policy(
        file: Option<&Path>,
        env: &dyn EnvSource,
        cli_overrides: &[(String, String)],
        policy: UnknownKeyPolicy,
    ) -> Result<LoadedConfig, ConfigError> {
        let mut config = Self::default();
        let mut warnings = Vec::new();

        if let Some(path) = file {
            let raw = std::fs::read_to_string(path).map_err(|error| {
                ConfigError::new(
                    ConfigErrorKind::FileUnreadable,
                    path.display().to_string(),
                    error.to_string(),
                )
            })?;
            let table: toml::Table = raw.parse().map_err(|error: toml::de::Error| {
                ConfigError::new(
                    ConfigErrorKind::Syntax,
                    path.display().to_string(),
                    error.message().to_owned(),
                )
            })?;
            for (key, value) in flatten(&table) {
                match config.apply(&key, &value) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ConfigErrorKind::UnknownKey => {
                        record_unknown(policy, error, &mut warnings)?;
                    }
                    Err(error) => return Err(error),
                }
            }
        }

        for key in CONFIG_KEYS {
            if let Some(value) = env.get(&env_var_for_key(key)) {
                config.apply(key, &value)?;
            }
        }

        for (key, value) in cli_overrides {
            config.apply(key, value)?;
        }

        // Unknown `ORBISYNC_*` variables are reported last, once the final
        // configuration is known: a secret variable is named by configuration
        // (`ORBISYNC_TOKEN_SIGNING_KEY` by default) and carries a value rather
        // than a configuration key, so it is not an unknown key.
        for name in env.names_with_prefix(ENV_PREFIX) {
            let is_config_key = CONFIG_KEYS.iter().any(|key| env_var_for_key(key) == name);
            let is_secret = config
                .required_secret_env_vars()
                .iter()
                .any(|secret| *secret == name);
            if is_config_key || is_secret || RESERVED_ENV_VARS.contains(&name.as_str()) {
                continue;
            }
            let error = ConfigError::new(
                ConfigErrorKind::UnknownKey,
                name,
                "environment variable does not map to a configuration key",
            );
            record_unknown(policy, error, &mut warnings)?;
        }

        config.validate()?;
        Ok(LoadedConfig { config, warnings })
    }

    /// Verifies that every required secret is present in the environment.
    ///
    /// Only presence is checked; the value is never logged or returned
    /// (`observability-and-config.md` §6.4).
    ///
    /// # Errors
    ///
    /// Returns [`ConfigErrorKind::MissingSecret`] naming the first missing
    /// environment variable.
    pub fn verify_secrets(&self, env: &dyn EnvSource) -> Result<(), ConfigError> {
        for name in self.required_secret_env_vars() {
            let present = env.get(name).is_some_and(|value| !value.trim().is_empty());
            if !present {
                return Err(ConfigError::new(
                    ConfigErrorKind::MissingSecret,
                    name,
                    "required secret environment variable is not set",
                ));
            }
        }
        Ok(())
    }
}

fn record_unknown(
    policy: UnknownKeyPolicy,
    error: ConfigError,
    warnings: &mut Vec<String>,
) -> Result<(), ConfigError> {
    match policy {
        UnknownKeyPolicy::Warn => {
            warnings.push(error.to_string());
            Ok(())
        }
        UnknownKeyPolicy::Reject => Err(error),
    }
}

/// Flattens a TOML table into dotted keys with textual values.
///
/// Scalars become their textual rendering so that file values, environment
/// values and CLI values share one parsing path.
fn flatten(table: &toml::Table) -> Vec<(String, String)> {
    fn walk(prefix: &str, table: &toml::Table, out: &mut Vec<(String, String)>) {
        for (name, value) in table {
            let key = if prefix.is_empty() {
                name.clone()
            } else {
                format!("{prefix}.{name}")
            };
            match value {
                toml::Value::Table(inner) => walk(&key, inner, out),
                toml::Value::String(text) => out.push((key, text.clone())),
                other => out.push((key, other.to_string())),
            }
        }
    }

    let mut out = Vec::new();
    walk("", table, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{Config, MapEnv, UnknownKeyPolicy};
    use crate::error::ConfigErrorKind;
    use std::io::Write as _;

    fn write_config(contents: &str) -> tempfile::NamedTempFile {
        let mut file = tempfile::Builder::new()
            .suffix(".toml")
            .tempfile()
            .expect("temp file");
        file.write_all(contents.as_bytes()).expect("write");
        file.flush().expect("flush");
        file
    }

    #[test]
    fn test_defaults_load_without_any_source() {
        let env = MapEnv::default();
        let loaded = Config::load(None, &env, &[]).expect("defaults are valid");
        assert_eq!(loaded.config, Config::default());
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn test_repository_example_configuration_loads() {
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").expect("manifest directory");
        let path = Path::new(&manifest_dir).join("../../orbisync.toml.example");
        let loaded = Config::load(Some(&path), &MapEnv::default(), &[])
            .expect("repository example must be a valid configuration");
        assert_eq!(loaded.config.server.max_request_body_bytes, 2_097_152);
        assert_eq!(loaded.config.server.request_timeout_seconds, 30);
        assert_eq!(loaded.config.server.page_default_limit, 50);
        assert_eq!(loaded.config.server.page_max_limit, 200);
        assert!(loaded.config.cors.allowed_origins.is_empty());
    }

    #[test]
    fn test_discover_path_prefers_cli_then_environment() {
        let env = MapEnv::from_pairs([("ORBISYNC_CONFIG_FILE", "/tmp/from-env.toml")]);
        assert_eq!(
            Config::discover_path(Some(Path::new("/tmp/from-cli.toml")), &env)
                .expect("CLI path is valid"),
            Some(PathBuf::from("/tmp/from-cli.toml"))
        );
        assert_eq!(
            Config::discover_path(None, &env).expect("environment path is valid"),
            Some(PathBuf::from("/tmp/from-env.toml"))
        );
    }

    #[test]
    fn test_discover_path_finds_local_config() {
        let original = std::env::current_dir().expect("current directory");
        let directory = tempfile::tempdir().expect("temporary directory");
        std::fs::write(
            directory.path().join("orbisync.toml"),
            "[server]\nbind = \"127.0.0.1:1\"\n",
        )
        .expect("local config");
        std::env::set_current_dir(directory.path()).expect("change directory");
        let discovered = Config::discover_path(None, &MapEnv::default()).expect("discover");
        std::env::set_current_dir(original).expect("restore directory");
        assert_eq!(discovered, Some(directory.path().join("orbisync.toml")));
    }

    #[test]
    fn test_explicit_missing_path_is_not_replaced_by_defaults() {
        let missing = PathBuf::from("/tmp/orbisync-missing-config.toml");
        let env = MapEnv::default();
        let error = Config::load(Some(&missing), &env, &[]).expect_err("missing path must fail");
        assert_eq!(error.kind(), ConfigErrorKind::FileUnreadable);
    }

    #[test]
    fn test_missing_cli_path_remains_authoritative_at_startup() {
        let missing = PathBuf::from("/tmp/orbisync-missing-cli-config.toml");
        let discovered = Config::discover_path(Some(&missing), &MapEnv::default())
            .expect("discover explicit path");
        let error = Config::load(discovered.as_deref(), &MapEnv::default(), &[])
            .expect_err("missing CLI path must fail");
        assert_eq!(error.kind(), ConfigErrorKind::FileUnreadable);
    }

    #[test]
    fn test_precedence_cli_beats_env_beats_file_beats_default() {
        let file = write_config(
            r#"
[server]
bind = "10.0.0.1:1111"

[database]
max_connections = 5
"#,
        );
        let env = MapEnv::from_pairs([("ORBISYNC_SERVER_BIND", "10.0.0.2:2222")]);
        let overrides = vec![("server.bind".to_owned(), "10.0.0.3:3333".to_owned())];

        let loaded =
            Config::load(Some(file.path()), &env, &overrides).expect("configuration is valid");
        assert_eq!(loaded.config.server.bind, "10.0.0.3:3333");
        // The file still wins over the default for keys nobody overrides.
        assert_eq!(loaded.config.database.max_connections, 5);
        // Untouched keys keep their default.
        assert_eq!(
            loaded.config.world.server_tick_hz,
            Config::default().world.server_tick_hz
        );
    }

    #[test]
    fn test_file_values_are_type_checked() {
        let file = write_config("[database]\nmax_connections = \"many\"\n");
        let env = MapEnv::default();
        let error = Config::load(Some(file.path()), &env, &[]).expect_err("type error");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
        assert_eq!(error.key(), "database.max_connections");
    }

    /// ADR-024: `world.speed_acceleration_check_enabled` is settable from a
    /// TOML file, the same source `orbisync.toml.example` uses, not only
    /// from the environment.
    #[test]
    fn test_speed_acceleration_check_enabled_is_settable_from_a_toml_file() {
        let file = write_config("[world]\nspeed_acceleration_check_enabled = false\n");
        let env = MapEnv::default();
        let loaded = Config::load(Some(file.path()), &env, &[]).expect("valid configuration");
        assert!(!loaded.config.world.speed_acceleration_check_enabled);
    }

    /// ADR-024: a non-boolean TOML value for the flag fails startup with the
    /// correct offending key, mirroring `test_file_values_are_type_checked`.
    #[test]
    fn test_speed_acceleration_check_enabled_file_value_is_type_checked() {
        let file = write_config("[world]\nspeed_acceleration_check_enabled = \"yes\"\n");
        let env = MapEnv::default();
        let error = Config::load(Some(file.path()), &env, &[]).expect_err("type error");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
        assert_eq!(error.key(), "world.speed_acceleration_check_enabled");
    }

    /// ADR-025: the pre-commit validation hook keys are settable from a TOML
    /// file, the same source `orbisync.toml.example` uses, not only from the
    /// environment.
    #[test]
    fn test_pre_commit_validation_keys_are_settable_from_a_toml_file() {
        let file = write_config(
            "[extensions]\npre_commit_validation_timeout_ms = 250\npre_commit_validation_max_concurrency = 4\npre_commit_additional_ca_path = \"/tmp/local-ca.pem\"\nallow_loopback_endpoints = true\n",
        );
        let env = MapEnv::default();
        let loaded = Config::load(Some(file.path()), &env, &[]).expect("valid configuration");
        assert_eq!(
            loaded.config.extensions.pre_commit_validation_timeout_ms,
            250
        );
        assert_eq!(
            loaded
                .config
                .extensions
                .pre_commit_validation_max_concurrency,
            4
        );
        assert!(loaded.config.extensions.allow_loopback_endpoints);
        assert_eq!(
            loaded.config.extensions.pre_commit_additional_ca_path,
            "/tmp/local-ca.pem"
        );
    }

    /// ADR-025: a non-boolean TOML value for `allow_loopback_endpoints` fails
    /// startup with the correct offending key, mirroring
    /// `test_speed_acceleration_check_enabled_file_value_is_type_checked`.
    #[test]
    fn test_pre_commit_validation_allow_loopback_file_value_is_type_checked() {
        let file = write_config("[extensions]\nallow_loopback_endpoints = \"yes\"\n");
        let env = MapEnv::default();
        let error = Config::load(Some(file.path()), &env, &[]).expect_err("type error");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
        assert_eq!(error.key(), "extensions.allow_loopback_endpoints");
    }

    #[test]
    fn test_unknown_file_key_warns_by_default_and_rejects_in_strict_mode() {
        let file = write_config("[server]\nbind = \"0.0.0.0:8080\"\nmystery = 1\n");
        let env = MapEnv::default();

        let loaded = Config::load(Some(file.path()), &env, &[]).expect("warning only");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("server.mystery"));

        let error =
            Config::load_with_policy(Some(file.path()), &env, &[], UnknownKeyPolicy::Reject)
                .expect_err("strict mode rejects");
        assert_eq!(error.kind(), ConfigErrorKind::UnknownKey);
    }

    #[test]
    fn test_secret_and_reserved_variables_are_not_unknown_keys() {
        let env = MapEnv::from_pairs([
            ("ORBISYNC_TOKEN_SIGNING_KEY", "development-only-key"),
            ("ORBISYNC_CONFIG_FILE", "/etc/orbisync/orbisync.toml"),
        ]);
        let loaded = Config::load(None, &env, &[]).expect("valid");
        assert!(
            loaded.warnings.is_empty(),
            "unexpected warnings: {:?}",
            loaded.warnings
        );
    }

    #[test]
    fn test_secret_variable_renamed_by_the_file_is_not_an_unknown_key() {
        let file = write_config(
            "[auth]
token_signing_key_env = \"ORBISYNC_CUSTOM_KEY\"
",
        );
        let env = MapEnv::from_pairs([("ORBISYNC_CUSTOM_KEY", "development-only-key")]);
        let loaded = Config::load(Some(file.path()), &env, &[]).expect("valid");
        assert!(loaded.warnings.is_empty());
    }

    #[test]
    fn test_unknown_environment_variable_is_reported() {
        let env = MapEnv::from_pairs([("ORBISYNC_SERVER_MYSTERY", "1")]);
        let loaded = Config::load(None, &env, &[]).expect("warning only");
        assert_eq!(loaded.warnings.len(), 1);
        assert!(loaded.warnings[0].contains("ORBISYNC_SERVER_MYSTERY"));
    }

    #[test]
    fn test_environment_overrides_are_validated() {
        let env = MapEnv::from_pairs([("ORBISYNC_WORLD_SERVER_TICK_HZ", "600")]);
        let error = Config::load(None, &env, &[]).expect_err("out of range");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
        assert_eq!(error.key(), "world.server_tick_hz");
    }

    #[test]
    fn test_verify_secrets_requires_named_variables() {
        let config = Config::default();
        let empty = MapEnv::default();
        let error = config
            .verify_secrets(&empty)
            .expect_err("secret is missing");
        assert_eq!(error.kind(), ConfigErrorKind::MissingSecret);
        assert_eq!(error.key(), "DATABASE_URL");

        let complete = MapEnv::from_pairs([
            ("DATABASE_URL", "postgres://localhost/orbisync"),
            ("ORBISYNC_TOKEN_SIGNING_KEY", "unit-test-key"),
            ("ORBISYNC_PAGINATION_HMAC_KEY", "unit-test-pagination-key"),
            ("ORBISYNC_REFRESH_TOKEN_HMAC_KEY", "unit-test-refresh-key"),
            (
                "ORBISYNC_REALTIME_TICKET_HMAC_KEY",
                "unit-test-realtime-ticket-key",
            ),
            ("ORBISYNC_IDEMPOTENCY_HMAC_KEY", "unit-test-idempotency-key"),
            (
                "ORBISYNC_EXTENSION_TOKEN_HMAC_KEY",
                "unit-test-extension-key-of-32-bytes",
            ),
        ]);
        config.verify_secrets(&complete).expect("secrets present");
    }

    #[test]
    fn test_secret_values_never_appear_in_the_configuration_model() {
        let file = write_config("[database]\nurl_env = \"CUSTOM_DATABASE_URL\"\n");
        let env = MapEnv::from_pairs([("CUSTOM_DATABASE_URL", "postgres://user:pw@host/db")]);
        let loaded = Config::load(Some(file.path()), &env, &[]).expect("valid");
        let rendered = format!("{:?}", loaded.config);
        assert!(!rendered.contains("postgres://"));
        assert!(rendered.contains("CUSTOM_DATABASE_URL"));
    }
}

#[cfg(test)]
mod auth_method_source_tests {
    use super::{Config, MapEnv};
    use crate::error::ConfigErrorKind;

    fn write_config(contents: &str) -> tempfile::NamedTempFile {
        use std::io::Write as _;
        let mut file = tempfile::NamedTempFile::new().expect("temp file");
        file.write_all(contents.as_bytes()).expect("write");
        file.flush().expect("flush");
        file
    }

    #[test]
    fn nested_method_tables_load_from_a_toml_file() {
        let file = write_config(
            r#"
[auth]
methods = ["local", "guest"]

[auth.guest]
role_names = ["Visitor"]
allowed_worlds = ["0192d43d-a18a-7fed-8123-0123456789ab"]
session_ttl_seconds = 1800
"#,
        );
        let loaded =
            Config::load(Some(file.path()), &MapEnv::default(), &[]).expect("valid configuration");
        assert_eq!(loaded.config.auth.methods, vec!["local", "guest"]);
        assert_eq!(loaded.config.auth.guest.role_names, vec!["Visitor"]);
        assert_eq!(loaded.config.auth.guest.session_ttl_seconds, 1800);
    }

    #[test]
    fn environment_overrides_the_method_list_and_nested_settings() {
        let file = write_config(
            r#"
[auth]
methods = ["local"]

[auth.guest]
role_names = ["FromFile"]
allowed_worlds = ["0192d43d-a18a-7fed-8123-0123456789ab"]
"#,
        );
        let env = MapEnv::from_pairs([
            ("ORBISYNC_AUTH_METHODS", "local,guest"),
            ("ORBISYNC_AUTH_GUEST_ROLE_NAMES", "FromEnv"),
        ]);
        let loaded =
            Config::load(Some(file.path()), &env, &[]).expect("environment override is valid");
        assert_eq!(loaded.config.auth.methods, vec!["local", "guest"]);
        assert_eq!(loaded.config.auth.guest.role_names, vec!["FromEnv"]);
    }

    #[test]
    fn enabling_a_method_from_the_environment_still_validates_its_settings() {
        // Turning guest on via the environment must not skip the checks that
        // the file path performs: the world boundary is still required.
        let env = MapEnv::from_pairs([("ORBISYNC_AUTH_METHODS", "local,guest")]);
        let error =
            Config::load(None, &env, &[]).expect_err("guest without roles or worlds must fail");
        assert_eq!(error.kind(), ConfigErrorKind::InvalidValue);
    }
}
