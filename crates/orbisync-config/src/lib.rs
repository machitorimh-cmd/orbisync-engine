//! Configuration loading, precedence and validation.
//!
//! `observability-and-config.md` §6 defines the contract implemented here:
//!
//! - precedence is CLI arguments > environment variables > configuration file >
//!   safe defaults (specification §28.3);
//! - every value is validated at startup and an invalid value aborts the boot
//!   (specification §28.1);
//! - unknown keys are reported as warnings, or as errors in strict mode;
//! - secrets are never stored in the configuration file. The file names the
//!   environment variable that carries the secret, and the loader only checks
//!   that the variable is present (§6.4).
//!
//! # Dependency rule
//!
//! `config` depends on no other workspace crate (`repo-crate-conventions.md`
//! §3.2), so it can be loaded before any domain or adapter type exists.
//!
//! # Example
//!
//! ```
//! use orbisync_config::{Config, EnvSource, MapEnv};
//!
//! let env = MapEnv::from_pairs([("ORBISYNC_SERVER_BIND", "127.0.0.1:9090")]);
//! let loaded = Config::load(None, &env, &[]).expect("defaults are valid");
//! assert_eq!(loaded.config.server.bind, "127.0.0.1:9090");
//! ```

mod error;
mod keys;
mod model;
mod source;

pub use error::{ConfigError, ConfigErrorKind};
pub use keys::{CONFIG_KEYS, ENV_PREFIX, env_var_for_key};
pub use model::{
    AuthConfig, Config, CorsConfig, DatabaseConfig, ExtensionsConfig, IdentityConfig,
    InterestConfig, LogFormat, LogLevel, ObservabilityConfig, RateLimitConfig, RealtimeConfig,
    RetentionConfig, ServerConfig, WorldConfig, parse_trusted_proxies,
};
pub use source::{EnvSource, LoadedConfig, MapEnv, SystemEnv, UnknownKeyPolicy};
