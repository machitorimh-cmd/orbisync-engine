# Deploy an application rule with the stock engine

There are three separately deployed parts: your game frontend uses the SDK;
your application rule service computes proposals over HTTPS JSON; the stock
OrbiSync server authenticates players, reads canonical state, validates proposals,
commits and delivers canonical updates. No engine source or Rust compilation is
needed to create a new application rule. Rule services may use any language with
HTTPS/JSON and HMAC support; Python below is only a runnable example. Client
language is independent: the available SDK is TypeScript/JavaScript, while
[CLIENT-WIRE.md](CLIENT-WIRE.md) and bundled public schemas support implementing
clients in other languages. No SDK for every language is implied.
The operator must install a server
release supporting `--input-rules`; older binaries cannot load these bindings.

The kit includes `external-input-python/service.py` (Python 3.11+, no third-party
packages), a manifest template, SDK tarball, starter, integration brief, server
configuration reference and this contract. The example is application code,
not a built-in engine rule. Replace its computation and publish your own intent
and canonical component schemas for other games. The SDK's generic
`instance.sendInput` accepts the provisioned rule name without engine sources.

## Operator provisioning

Use your existing stock-server installation, PostgreSQL database, migrations,
TLS ingress, authentication secrets and approved password corpus. The included
`orbisync.toml.example` documents the supported configuration, including the
Ed25519 PKCS#8 PEM access-token signing key and separate HMAC secrets. Supply
`DATABASE_URL`, those referenced secrets, and `ORBISYNC_PASSWORD_DENYLIST_FILE`
through your deployment's secret/configuration manager. Keep
`realtime.allow_stub_ticket = false`. Configure CORS for your actual frontend
origin. Do not enable stub authentication for this setup.

For a fresh installation, these are existing stock commands:

```sh
orbisync-server --config orbisync.toml migrate
orbisync-server --config orbisync.toml bootstrap-admin \
  --login-id YOUR_ADMIN_LOGIN --display-name YOUR_ADMIN_NAME \
  --password-denylist /secure/password-corpus.txt \
  --password-output /secure/new-admin-password.txt
orbisync-server --config orbisync.toml serve
```

Bootstrap runs once and writes a new owner-only password file. The corpus must
contain exactly 10,000 distinct approved lines; it is operator-owned, not a
dummy kit file. Existing installations use existing authorized administrators.

Provision through these public interfaces. JSON writes use
`Content-Type: application/json`; authenticated calls use
`Authorization: Bearer <access_token>`. Supply a fresh canonical lowercase
UUIDv7 `Idempotency-Key` on administrative POST writes and password changes;
preserve the same key/body when retrying an uncertain write. Never embed admin
credentials in the game or rule service.

| Responsibility | Existing interface and body |
| --- | --- |
| Discover enabled authentication | `GET /v1/auth/methods` |
| Administrator/player local login | `POST /v1/auth/login` with `{"login_id":"...","password":"..."}`; response contains `access_token` |
| Change a temporary password | `POST /v1/auth/change-password` with `{"current_password":"...","new_password":"..."}`; log in again afterwards |
| Create a local player | Admin `POST /v1/users` with `{"login_id":"...","display_name":"..."}` and `Accept: application/vnd.orbisync.user-credential+json`; response has `user.id` and `temporary_password`, deliver securely |
| Define a player role | Admin `POST /v1/roles` with `{"name":"game-player","permissions":["world.instance.read","entity.spawn","entity.update.own"]}`; save returned `id` |
| Assign the role | Admin `PUT /v1/users/{user_id}/roles` with `{"role_ids":["ROLE_UUID"]}` (replaces the whole role set) |
| Create world definition | Authorized `POST /v1/worlds` with `{"name":"Your game","capacity":100}`; save returned `id` as **world ID** |
| Create its instance | Authorized `POST /v1/instances` with `{"world_id":"WORLD_UUID","capacity":100}`; save returned `id` as **instance ID** |
| Start the instance | Authorized `POST /v1/instances/{instance_id}/start` |

World creation requires `admin.worlds.create`; instance creation/start require
`world.instance.create` / `world.instance.start`. These are operator permissions,
not requirements for ordinary players. Guest/name-only/external authentication
is also supported if the operator enables and configures it using the supplied
configuration reference. Guest/name-only roles and allowed-world UUIDs must be
provisioned before issuing identities. Rule registration grants no player roles.

Connect as the player with `connectSession` (see README). It handles tickets and
the WebSocket protocol. Use the existing reliable entity command to create an
owned entity when that player has `entity.spawn`:

```ts
const spawnCommandId = session.instance.sendEntityCommand({
  entityId: applicationAllocatedUuidV7,
  operation: "spawn", expectedRevision: 0n,
  args: { kind: "object", visibility: "global",
          position_x: 0, position_y: 0, position_z: 0 },
});
```

Allocate a fresh UUIDv7 with your application's UUID library. The requester owns
the spawned object. Subscribe to canonical state and errors before sending;
wait until the entity appears before sending input. `sendEntityCommand` returns
a command ID, not a success promise. This example uses global visibility so a
second authorized client in the same instance can see the update. The supplied
CLI expects that entity to exist already; it does not do this provisioning.

## Register and start the rule service

1. Copy `external-input-python/input-rules.example.json` to `input-rules.json`.
   Set the real world UUID and the deployed HTTPS endpoint. Choose a stable
   rule name and an application component key. The included service implements
   `example.move` / `example.position` with intent `{dx:number}`, -1 <= dx <= 1.
2. Generate a strong shared secret in your secret manager. Inject the same
   UTF-8 value as `ORBISYNC_RULE_SECRET` into both the engine and this example
   service. Only the engine's environment-variable reference goes in the
   manifest; no secret or endpoint belongs in frontend configuration.
3. Start the example behind your HTTPS reverse proxy, preserving raw request
   bodies and `X-OrbiSync-*` headers:

   ```sh
   python external-input-python/service.py --world-id WORLD_UUID
   ```

   It listens on `127.0.0.1:9443`. Alternatively, serve TLS directly with
   `--bind 0.0.0.0 --cert /secure/fullchain.pem --key /secure/private-key.pem`.
   Deploy a certificate trusted by the engine and DNS reachable under its
   extension egress policy. HTTPS is required even for development calls.
4. Register on the stock engine by starting/restarting it with:

   ```sh
   orbisync-server --config orbisync.toml --input-rules input-rules.json serve
   ```

   `ORBISYNC_INPUT_RULES_FILE=/absolute/path/input-rules.json` is the equivalent
   environment option. Apply identical bindings/secrets to every serving node.
   Registration is process startup configuration, not a client upload endpoint
   or a database extension row. Binding changes require restart. A world/rule
   pair and world/component pair may each appear only once. Invalid manifests,
   duplicate bindings, invalid endpoint syntax or missing secrets fail startup.
5. Give the frontend its normal connection/authentication values plus the rule
   contract and owned entity ID. Call
   `instance.sendInput({entityId, rule:"example.move", intent:{dx:1}})` and await
   the returned `result`. No endpoint or signing key is sent by the player.

This reuses extension HTTPS transport: no redirects/proxies, DNS/IP SSRF checks,
certificate validation, and `extensions.pre_commit_additional_ca_path` when a
private CA is needed. `extensions.allow_loopback_endpoints` is only for local
development, never production. Literal private/loopback-IP endpoints are rejected;
development uses a hostname resolving to loopback plus that explicit option.
Existing precommit timeout/concurrency settings also bound external computation
(a separate process-wide input-rule concurrency pool). The realtime foreground
deadline still applies. These bounds are not game latency or capacity guarantees.

## HTTPS JSON contract v1

Engine POSTs an existing signed extension envelope:

```json
{
  "event_id": "COMMAND_UUID", "event_kind": "input.compute", "timestamp": 1700000000,
  "payload": {
    "version": 1, "request_id": "COMMAND_UUID", "world_id": "WORLD_UUID",
    "instance_id": "INSTANCE_UUID", "requester": "AUTHENTICATED_USER_UUID",
    "rule": "example.move", "component_key": "example.position",
    "now_unix_ms": "1700000000000",
    "current_entity": {
      "entity_id": "ENTITY_UUID", "revision": "2", "owner_id": "OWNER_UUID",
      "components": {"example.position": {"encoding": "json", "value": {"component_key":"example.position","x":1}}}
    },
    "intent": {"dx": 1}
  }
}
```

`requester` comes from authenticated engine context; canonical state comes from
the actor, never the player. Revisions and millisecond time are decimal strings
to preserve precision across languages. `owner_id` may be null. Non-JSON
components use `{"encoding":"base64","value":"..."}`. Engine `core.*`
components are omitted. Custom component data retains its stored JSON shape.

Verify the raw body before parsing:

* `X-OrbiSync-Event-Id` = command UUID.
* `X-OrbiSync-Timestamp` = signature timestamp in Unix seconds.
* `X-OrbiSync-Signature` = `sha256=` plus lowercase HMAC-SHA256 hex of UTF-8
  `timestamp + "." + event_id + "." + raw_body`, using the shared secret.

Compare signatures in constant time, check header/body correlation and timestamp
freshness (example: 60 seconds; synchronize clocks). Verify expected world/rule/
component as the example does. Signature timestamp is delivery time; game state
time is `now_unix_ms`. Responses are authenticated by HTTPS, not an extra HMAC.

Return HTTP 2xx with exactly one correlated proposal or rejection:

```json
{"version":1,"request_id":"COMMAND_UUID","decision":"accept","update":{"x":2}}
```

```json
{"version":1,"request_id":"COMMAND_UUID","decision":"reject","reason":"step out of range"}
```

`update` is an object containing the entire replacement component payload, not
a patch. Do not include `component_key` or `key`: the engine inserts the bound
component selector. JSON numbers must be finite and within the safe binary64
range (absolute value <= 2^53-1); use strings for larger integers. An accepted
response must omit `reason`; a rejection must omit `update`. Unknown fields,
versions, decisions, correlation mismatches, malformed bodies, unavailable
services and non-2xx responses fail closed without mutation. Response bodies
have the existing 16 KiB transport limit; committed component JSON including
the inserted selector must fit the existing 4096-byte component limit and all
normal engine validation. No limits are expanded by registering a rule.

The engine reports computation failures/rejections as `INVALID_ARGUMENT` through
the existing input result, unless durability itself is unavailable. Reasons are
player-visible: never put secrets in them. A completed rejection is deduplicated
just like other command outcomes. Preserve the SDK-returned original request
for uncertain retries; never reuse its ID with changed intent or revision.

## Application numbers and integer fields

Protobuf `Struct` stores numbers as IEEE 754 binary64, with no separate integer
type. This applies to both client intent and canonical application component
state: after conversion through the engine, mathematically integral values may
appear in JSON as `0.0` or `1.0`, even if originally submitted as `0` or `1`.
A proposal's numbers can return in this form in the next request's
`current_entity.components` and in client state. Validate numeric value,
integrality and the field's range, not JSON spelling or Python's exact `int`
type. Apply this to application state/version fields as well as intent fields.
See [CLIENT-WIRE.md](CLIENT-WIRE.md) for the client transport and state contract.

For integer-valued application fields carried as numbers, the interoperable safe
integer range is `-(2**53 - 1)` through `2**53 - 1`, inclusive. Larger integers
need an application-agreed string encoding; validation after binary64 rounding
cannot recover the original integer. Apply any narrower bounds from your own
field schema. Booleans, fractions, nonfinite numbers and out-of-range values
remain invalid for integer fields. This does not make all application numbers
integers: the supplied movement example deliberately permits fractional steps.
Dedicated protobuf integer fields and the decimal-string revision/time fields
above retain their separate contracts.

For example, with Python's standard JSON parser:

```python
import json
import math

SAFE_INTEGER = 2**53 - 1

def integer_field(value, minimum=-SAFE_INTEGER, maximum=SAFE_INTEGER):
    # bool is an int subclass; reject it before normalizing.
    if type(value) not in (int, float):
        raise ValueError("expected a number")
    if isinstance(value, float) and (
        not math.isfinite(value) or not value.is_integer()
    ):
        raise ValueError("expected a finite integral number")
    if not -SAFE_INTEGER <= value <= SAFE_INTEGER or not minimum <= value <= maximum:
        raise ValueError("integer out of range")
    return int(value)  # Normalize only after validation, before indexing/arithmetic.

intent = json.loads('{"index": 0.0}')
state = json.loads('{"version": 1.0, "count": 2.0}')
index = integer_field(intent["index"], minimum=0)
version = integer_field(state["version"], minimum=1, maximum=1)
count = integer_field(state["count"], minimum=0)
assert (index, version, count) == (0, 1, 2)
```

## Computation and commit boundary

A service returns a **proposal**, not a commit receipt. Core rechecks permissions
and participation, enforces exact observed entity state at commit, runs existing
precommit validation, persists under its configured durability mode and delivers
the canonical result to the sender and eligible peers. A concurrent change may
discard a computed proposal. Registered components reject direct client update
commands; unrelated collaboration components retain their existing behavior.

Rules must be pure computations of the supplied context/state/intent: no payment,
email, database mutation, external gameplay commit, or other irreversible side
effect. Calls can repeat after interruption/retry; a service must not assume
exactly-once execution. Completed engine commands replay their original result
without applying again. Do not cache a proposal using command ID alone across
different supplied states. Use established postcommit event delivery for side
effects where available, with the receiver's own idempotency.

Local Rust rules and deny-only precommit extensions remain supported. This
contract handles one existing entity and one registered component per command.
It does not provide multi-entity transactions, physics simulation, arbitrary
plugin upload, rule-service hosting, or automatic game/user/world provisioning.
