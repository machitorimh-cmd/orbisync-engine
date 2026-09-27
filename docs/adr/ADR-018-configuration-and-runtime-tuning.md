# ADR-018: Configuration discovery and runtime tuning

- Status: Accepted

## Context

OrbiSync had runtime limits distributed across adapters and services. Some of
those limits were not visible in the typed configuration model, and the server
did not consistently report which configuration source it loaded. Operators
need deterministic discovery, explicit defaults, and a single source of truth
for concurrency, authentication, rate limiting, retention, import, and world
runtime settings.

## Decision

1. Configuration discovery uses this precedence: explicit CLI path,
   `ORBISYNC_CONFIG_FILE`, `./orbisync.toml`,
   `/etc/orbisync/orbisync.toml`, then built-in defaults. An explicitly named
   but missing file is an error rather than a fallback trigger.
2. All runtime-tunable values are represented in the typed configuration
   model, with validation and documented defaults. CLI overrides remain the
   final override layer.
3. The composition root owns wiring configuration into the runtime, storage,
   identity, HTTP, realtime, and retention adapters. Adapters retain safe
   defaults for direct construction in tests and libraries.
4. Startup emits a structured `config.loaded` event identifying the selected
   file, or `defaults` when no file was found. Secrets and secret values are
   never logged.
5. Runtime changes are limited to bounded resource and policy settings; they
   do not change protocol or persistence contracts. Invalid values fail closed
   during configuration validation.
6. `world.component_updates_per_sec` defaults to `10` and is enforced by the
   instance runtime's serialized component-command path. The runtime is the
   natural owner because it already validates entity ownership and can reject
   an update without disconnecting the transport. The value matches
   `rate_limit.custom_per_sec` and the 10 Hz position-update order of
   magnitude, so custom updates do not overwhelm the instance tick.
7. `core.transform`, `core.velocity`, `core.animation`, and `core.presence`
   are reserved names only. This milestone does not interpret their payloads:
   `Entity.transform` remains a separate field, and velocity/animation/
   presence behavior belongs to later runtime milestones. Keeping the names
   reserved now prevents clients from claiming server-owned namespaces while
   avoiding an out-of-scope schema or aggregate migration.

## Consequences

- Deployments can tune resource limits without source changes.
- Configuration usage can be audited because every declared key has a
  production reader.
- Direct adapter users must still pass explicit security-sensitive values when
  the constructor requires them; safe defaults are only compatibility aids.
- Changing a value can affect capacity, authentication cost, or cleanup load,
  so operational changes should be reviewed and observed after rollout.
