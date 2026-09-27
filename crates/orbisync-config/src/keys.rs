//! The configuration key schema.
//!
//! Every setting has exactly one dotted key. The environment variable name is
//! derived mechanically, so `ORBISYNC_<SECTION>_<KEY>` stays consistent with
//! the file layout (`observability-and-config.md` §6.1).

/// Prefix of every OrbiSync environment variable.
pub const ENV_PREFIX: &str = "ORBISYNC_";

/// Every configuration key, in file order.
///
/// The list is also the allowlist used to detect unknown keys in the file and
/// unknown `ORBISYNC_*` variables in the environment (specification §28.1).
pub const CONFIG_KEYS: &[&str] = &[
    "server.bind",
    "server.trusted_proxies",
    "server.worker_threads",
    "server.max_request_body_bytes",
    "server.request_timeout_seconds",
    "server.page_default_limit",
    "server.page_max_limit",
    "server.shutdown_drain_timeout_seconds",
    "server.shutdown_force_timeout_seconds",
    "cors.allowed_origins",
    "cors.allow_credentials",
    "database.url_env",
    "database.max_connections",
    "database.acquire_timeout_seconds",
    "database.readiness_timeout_seconds",
    "auth.access_token_ttl_seconds",
    "auth.refresh_token_ttl_seconds",
    "auth.password_min_length",
    "auth.token_signing_key_env",
    "auth.pagination_hmac_key_env",
    "auth.refresh_token_hmac_key_env",
    "auth.realtime_ticket_hmac_key_env",
    "auth.idempotency_hmac_key_env",
    "auth.allow_stub_bearer",
    "auth.argon2_memory_cost_kib",
    "auth.argon2_iterations",
    "auth.argon2_parallelism",
    "auth.password_hash_concurrency",
    "auth.login_failure_threshold",
    "auth.lockout_duration_seconds",
    "auth.methods",
    "auth.guest.role_names",
    "auth.guest.session_ttl_seconds",
    "auth.guest.retention_seconds",
    "auth.guest.allowed_worlds",
    "auth.guest.display_name_prefix",
    "auth.name_only.role_names",
    "auth.name_only.session_ttl_seconds",
    "auth.name_only.retention_seconds",
    "auth.name_only.allowed_worlds",
    "auth.name_only.display_name_prefix",
    "auth.external.issuer",
    "auth.external.audience",
    "auth.external.algorithm",
    "auth.external.jwks_path",
    "auth.external.leeway_seconds",
    "auth.external.role_names",
    "rate_limit.normal_per_sec",
    "rate_limit.custom_per_sec",
    "rate_limit.persistent_threshold",
    "rate_limit.max_buckets",
    "rate_limit.login_per_ip_per_minute",
    "rate_limit.login_ip_block_seconds",
    "rate_limit.login_ip_max_buckets",
    "rate_limit.realtime_ticket_per_interval",
    "realtime.heartbeat_interval_seconds",
    "realtime.connection_timeout_seconds",
    "realtime.max_normal_message_bytes",
    "realtime.max_message_bytes",
    "realtime.outbound_queue_capacity",
    "realtime.allow_stub_ticket",
    "realtime.handshake_timeout_ms",
    "realtime.max_connections",
    "realtime.per_connection_capacity",
    "realtime.resume_prune_interval_seconds",
    "world.default_capacity",
    "world.server_tick_hz",
    "world.resume_grace_seconds",
    "world.checkpoint_interval_secs",
    "world.checkpoint_interval_ticks",
    "world.checkpoint_chunk_bytes",
    "world.checkpoint_generation_enabled",
    "world.checkpoint_writer_lock",
    "world.checkpoint_deployment",
    "world.checkpoint_max_serialized_bytes",
    "world.history_capacity",
    "world.max_speed",
    "world.max_acceleration",
    "world.speed_acceleration_check_enabled",
    "world.component_updates_per_sec",
    "world.mailbox_control_capacity",
    "world.mailbox_transform_capacity",
    "world.mailbox_entity_capacity",
    "identity.csv_max_bytes",
    "identity.csv_max_rows",
    "retention.realtime_ticket_interval_seconds",
    "retention.idempotency_interval_seconds",
    "retention.drain_budget_seconds",
    "retention.restart_backoff_seconds",
    "retention.extension_outbox_days",
    "retention.source_ip_days",
    "extensions.delivery_timeout_seconds",
    "extensions.max_concurrency",
    "extensions.max_attempts",
    "extensions.backoff_min_seconds",
    "extensions.backoff_max_seconds",
    "extensions.circuit_failure_threshold",
    "extensions.circuit_open_seconds",
    "extensions.dlq_retention_days",
    "extensions.pre_commit_validation_timeout_ms",
    "extensions.pre_commit_validation_max_concurrency",
    "extensions.pre_commit_additional_ca_path",
    "extensions.allow_loopback_endpoints",
    "interest.cell_size",
    "interest.near_radius",
    "interest.unsubscribe_radius",
    "observability.log_format",
    "observability.log_level",
    "observability.audit_retention_days",
];

/// Returns the environment variable that overrides `key`.
///
/// ```
/// use orbisync_config::env_var_for_key;
///
/// assert_eq!(env_var_for_key("server.bind"), "ORBISYNC_SERVER_BIND");
/// ```
#[must_use]
pub fn env_var_for_key(key: &str) -> String {
    format!("{ENV_PREFIX}{}", key.replace('.', "_").to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{CONFIG_KEYS, env_var_for_key};
    use std::collections::HashSet;

    #[test]
    fn test_config_keys_are_unique() {
        let unique: HashSet<&&str> = CONFIG_KEYS.iter().collect();
        assert_eq!(unique.len(), CONFIG_KEYS.len());
    }

    #[test]
    fn test_env_var_names_are_unique_and_prefixed() {
        let names: HashSet<String> = CONFIG_KEYS.iter().map(|key| env_var_for_key(key)).collect();
        assert_eq!(names.len(), CONFIG_KEYS.len());
        assert!(names.iter().all(|name| name.starts_with("ORBISYNC_")));
    }

    #[test]
    fn test_env_var_for_key_follows_section_key_naming() {
        assert_eq!(
            env_var_for_key("realtime.heartbeat_interval_seconds"),
            "ORBISYNC_REALTIME_HEARTBEAT_INTERVAL_SECONDS"
        );
        assert_eq!(
            env_var_for_key("database.max_connections"),
            "ORBISYNC_DATABASE_MAX_CONNECTIONS"
        );
    }
}
