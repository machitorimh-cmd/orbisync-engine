# ADR-024: Speed/acceleration check toggle

- Status: Accepted

## Context

`state-and-runtime.md` §2.2 requires validating maximum speed and
acceleration (specification §11.2, §11.3), and `instance_runtime` enforces
this unconditionally via `validate_transform_update_with_limits`
(`crates/orbisync-world-runtime/src/validation.rs`). The check combines
speed and acceleration into a single comparison:
`speed_limit = max_speed + max_acceleration * delta_seconds`. There is no
independent acceleration-only judgement — `max_acceleration` only widens the
speed ceiling for the interval. There was previously no way to disable this
comparison; raising `world.max_speed` / `world.max_acceleration` to a very
large value is not an equivalent, verified substitute, because their
interaction with `delta_seconds` clamping (`MIN_DELTA_SECONDS`) at extreme
values is untested and their `is_finite() && > 0.0` validation does not
establish any upper bound.

OrbiSync Core's stated goal (`README.md`) is a headless backend usable for
purposes beyond games, including applications whose own logic (e.g. an
external flight-physics engine driving a drone simulator) already validates
kinematics. For such use cases, Core's speed/acceleration check can reject
otherwise-valid updates. At the same time, OrbiSync's server-authority model
(`state-and-runtime.md` §2.1, specification §11.1) and its other minimum
validations — authenticated session, membership, ownership, numeric
finiteness, update-rate limiting, message-size limiting, protocol-version
compatibility — are Core invariants that must hold regardless of use case.
The per-tick teleport distance check and world-boundary check are out of
scope for this decision; their existing behavior is unchanged.

## Decision

Add `world.speed_acceleration_check_enabled: bool` (default `true`) as a new
configuration key, independent of the existing `world.max_speed` /
`world.max_acceleration` thresholds. It gates only the speed/acceleration
comparison in `validate_transform_update_with_limits`. It does not gate:
delta-seconds finiteness, the per-tick teleport distance check
(`MAX_TICK_DISTANCE`), scale finiteness, ownership, `expected_revision`,
authentication, membership, update-rate limiting, message-size limiting, or
protocol-version compatibility — all of these remain enforced regardless of
this setting.

A single combined key is used for both speed and acceleration rather than
two independent keys, because `max_acceleration` has no standalone check to
gate independently; a separate `world.acceleration_check_enabled` key would
control nothing that isn't already controlled by the speed key.

Default is `true` (checked), matching current behavior, for two reasons:
(1) specification §11.2 lists maximum speed and acceleration among the
minimum validations, so silently defaulting to unchecked would diverge from
that baseline for deployments that do not opt in; (2) backward compatibility
— existing deployments that omit this key must observe unchanged behavior.
The trade-off is that a general-purpose deployment which does not need the
check must take an explicit action (set the key to `false`) rather than
getting permissive behavior for free; this is accepted because Core should
not assume any particular use case does not need server-side kinematics
validation, and a default-on posture fails closed rather than open.

Configuration wiring follows the existing plumbing for `max_speed` /
`max_acceleration`: `WorldConfig` (`crates/orbisync-config`) →
`RealtimeStateBuilder::with_speed_acceleration_check` →
`RealtimeState.speed_acceleration_check_enabled` → applied to every
`InstanceActor` at construction (fresh instance, checkpoint restore, and
row-only recovery) via `InstanceActor::with_speed_acceleration_check`, the
same builder-after-construct pattern already used for
`component_updates_per_sec`. The value is read once at process startup by
the composition root (`main.rs`); as with all other `world.*` settings,
changing it requires a server restart — there is no hot-reload path.

Omitting the key from the configuration file or environment defaults to
`true` (unchanged behavior). An invalid value (anything other than a
boolean literal accepted by the existing config parser) is rejected at
startup, consistent with how other `bool` keys such as
`realtime.allow_stub_ticket` are validated.

## Consequences

- A deployment can disable Core's speed/acceleration check for a use case
  whose own logic validates kinematics, without modifying Core.
- No other minimum validation from specification §11.2 is weakened by this
  setting.
- The per-tick teleport distance check and world-boundary check are
  unaffected; SR-01 (reject vs. clamp on speed/boundary excess,
  `state-and-runtime.md` §2.2) remains a separate, still-open decision.
- Acceleration cannot be gated independently of speed, because no
  independent acceleration check exists to gate. If a future use case needs
  an independent acceleration judgement, that is new validation logic, not
  a toggle on existing logic, and is out of scope for this ADR.
