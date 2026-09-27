# Language-independent client protocol

The engine protocol and external rule-service contract do not require a
particular game or programming language. This distribution provides a
TypeScript/JavaScript SDK; it does **not** promise an SDK for every language.
Other-language clients implement the public HTTP + binary WebSocket contracts
below using their language's HTTP, WebSocket and Protocol Buffers libraries.
Python is only the rule-service example, not a runtime requirement for rules.
The frontend and rule service may use different languages.

## Distributable schemas

The kit and SDK package's `dist` directory both contain:

* `protocol/orbisync/v1/realtime.proto`: complete public proto3 schema,
  package `orbisync.v1`, including field numbers and all message definitions.
* `protocol/http/orbisync-v1.yaml`: implemented public HTTP API, request/response
  schemas, authentication requirements, headers and error responses.
* `protocol/http/errors.yaml`: public error-code registry.
* This guide: binary encoding, handshake/session, state reduction and input
  receipt semantics that cannot be expressed by the protobuf schema alone.
* `EXTERNAL-RULES.md`: separate, signed engine-to-rule HTTPS JSON contract and
  operator registration/provisioning instructions.

Generate bindings for your client language from `realtime.proto` using a proto3
compiler and that language's protobuf runtime. Its only import is the standard
`google/protobuf/struct.proto` well-known type supplied by protobuf toolchains;
include your toolchain's well-known-type include directory when compiling.
`Struct` is a map of string keys to `Value` (null, boolean, binary64 number,
string, nested Struct or ListValue). It is encoded as protobuf on the socket,
not as a JSON string. No engine source, Rust compilation, repository-relative
imports or private test helper is required. The TypeScript SDK already contains
generated bindings and needs no consumer-side code generation.

Binary64 has no separate integer type. In both intent and canonical application
state, an integral value can round-trip to JSON as `0.0` or `1.0`. Validate
integer fields by numeric value, integrality and schema range, not JSON spelling
or a language's exact integer type; reject booleans, fractions, nonfinite and
out-of-range values. Numeric safe integers range from `-(2**53 - 1)` through
`2**53 - 1`; agree on strings for larger application integers. See
[Application numbers and integer fields](EXTERNAL-RULES.md#application-numbers-and-integer-fields)
for a Python example covering intent and canonical state. This does not change
the dedicated protobuf integer fields described below.

Use the protocol bundled with the release your operator runs. An endpoint must
negotiate the feature described below before the client relies on this state
and receipt contract. Schemas describe transport; the operator separately
supplies game-specific rule names, intent schemas and component schemas.

## HTTP authentication and tickets

Use the operator's HTTPS API base URL. Request/response bodies use UTF-8 JSON;
JSON requests use `Content-Type: application/json`. The OpenAPI file is the
machine-readable reference for the following paths:

| Request | Body / successful response |
| --- | --- |
| `GET /v1/auth/methods` | No credentials; `{"methods":["local", ...]}` |
| `POST /v1/auth/login` | `{"login_id":"...","password":"..."}` |
| `POST /v1/auth/guest` | `{}`; only when enabled |
| `POST /v1/auth/name` | `{"display_name":"..."}`; only when enabled |
| `POST /v1/auth/external` | `{"token":"ISSUER_TOKEN"}`; only for the configured external issuer |
| `POST /v1/auth/refresh` | `{"refresh_token":"..."}` |
| `POST /v1/realtime/tickets` | No body; `Authorization: Bearer ACCESS_TOKEN`; response `{"realtime_ticket":"...","expires_in":60}` (use returned lifetime) |
| `POST /v1/auth/logout` | No body; bearer access token; revokes that authentication session |

All successful authentication methods return an access/refresh token pair with
`token_type` and `expires_in` (seconds). Refresh rotates the refresh token:
atomically store the new pair and serialize refresh attempts. Refresh does not
extend a temporary identity beyond its configured absolute lifetime. A disabled
method returns `AUTH_METHOD_DISABLED`; do not silently change the player's
identity or authentication method. HTTP errors have an `error` object containing
`code`, `message`, `request_id`, and `details`; consult the bundled HTTP schema.

Request a **new single-use ticket for each socket connection**, including after
reconnect. It is bound to an active auth session and expires quickly. An access
token, rule signing secret, user UUID or resume token cannot replace a ticket.
On ticket HTTP 401, refresh once if possible then request a new ticket. Treat
401/403 after refresh as authentication failure; back off on 429 and transient
service failures. Never put credentials/tickets in a URL or logs.

World/instance creation, role assignment and entity ownership are separate from
authentication; use the public provisioning interfaces in EXTERNAL-RULES.md.
Do not invent a REST `sendInput` or entity-upload endpoint.

## WebSocket encoding and session

Connect to `wss://HOST/ws` (also `/v1/realtime/ws`) with WebSocket subprotocol
`orbisync.v1.protobuf`. Use `ws://` only for a local HTTP deployment. A WebSocket
**binary message contains exactly one serialized `orbisync.v1.Envelope`**:
no additional length prefix, JSON framing or base64 around the envelope. Text
WebSocket messages are not this protocol. Let the WebSocket library reassemble
WebSocket fragments before protobuf decoding.

Envelope headers for every client message:

| Field | Meaning |
| --- | --- |
| `protocol_major` | `1` |
| `protocol_minor` | `0` initially, then the negotiated minor |
| `message_id` | UUIDv7 string; for commands use the command ID to correlate errors |
| `sequence` | Client-direction per-connection monotonically increasing uint64, start at 1; server sequence is independent |
| `sent_at_unix_ms` | Unix milliseconds, int64 |
| `instance_id` | Joined instance UUID for instance traffic; empty during hello |
| `payload` | Exactly one schema-defined oneof message |

Keep uint64 revisions/sequences and int64 times exact in your language. They are
protobuf integers, not floating-point numbers. Snapshot JSON below also needs
lossless integer parsing. `Struct` numbers are binary64: encode large application
integers as strings by agreement with the application contract.

1. Immediately send `ClientHello` as sequence 1: minor range `0..0`, fresh
   `realtime_ticket`, application `client_name`/`client_version`/`client_type`,
   `supported_compressions: []`, `supported_features: ["orbisync.state-sync.v1"]`.
   Leave hello's `resume_token` empty; explicit resumption uses `ResumeSession`.
   No custom payload compression is requested by this contract.
2. Await `ServerHello`. Verify the negotiated version and that
   `enabled_features` includes `orbisync.state-sync.v1`. Without that feature,
   do not claim compatible canonical input/state support. Save its heartbeat
   interval and connection identity.
3. Send `JoinInstance {world_instance_id: INSTANCE_UUID}`. This is an instance
   UUID, not a world definition UUID. Await `JoinAccepted`, retain its presence,
   authenticated user, instance and resume token. The permission flags are
   informative; the server still authorizes each operation.
4. Assemble and validate the following complete snapshot before enabling input.
   JoinAccepted's nearby entities alone are not the ready-state boundary.
5. Send protobuf `Heartbeat {client_time_unix_ms: ...}` at the advertised
   heartbeat interval. Process `HeartbeatAck` (echoed client time plus server
   time). WebSocket Ping/Pong alone does not replace the application heartbeat.
   Detect missed acknowledgements, stop input and recover the connection.
6. To leave, close the WebSocket normally (code 1000), cancel heartbeats and
   pending state assembly, and stop issuing commands. Logout is a separate
   HTTP action if the application also wants to revoke its auth session.

## Canonical state and snapshot encoding

`Snapshot.data` chunks are consecutive **bytes of one UTF-8 JSON document**.
Group by connection generation, instance ID and snapshot ID. Require identical
`chunk_count` and `instance_revision`; indices are zero-based and must all be
present. Accept a byte-identical repeated chunk once, reject conflicting chunks.
Concatenate in index order **before** UTF-8 decoding; a codepoint may span chunks.
Bound retained bytes/chunks/time and abandon incomplete or invalid assembly.
The server currently sends chunks of at most 16 KiB; do not infer completion
from a short chunk. Completion is determined by `chunk_count`.

The JSON body has this format (illustrative values; property names are literal):

```json
{
  "format":"orbisync.snapshot.v1",
  "revision":12,
  "user":{"user_id":"USER_UUID"},
  "instance":{"instance_id":"INSTANCE_UUID","world_id":"WORLD_UUID",
              "capacity":100,"lifecycle":"running","world_name":"Your game"},
  "permissions":{"entity_spawn":true,"entity_update_own":true,"entity_update_any":false},
  "entities":[{"entity_id":"ENTITY_UUID","revision":3,"kind":"object",
    "transform":null,"velocity":null,
    "properties":{"game.state":{"encoding":"json","value":{"component_key":"game.state","score":7}}}}],
  "users":[{"presence_id":"PRESENCE_UUID"}],
  "server_time_unix_ms":1700000000000,
  "filtered":true
}
```

Require matching body/envelope instance and revision, valid unique entity IDs,
exact uint64 revision values and the expected `format`. Entity transform, when
present, contains `position:{x,y,z}`, `rotation:{x,y,z,w}`, `scale:{x,y,z}`;
velocity is `{x,y,z}`. Preserve supplied instance/user/permission metadata.
`properties` may be null/empty. Each custom property uses
`{encoding:"json",value:APPLICATION_JSON}` or `{encoding:"base64",value:BASE64}`.
Decode base64 as opaque component bytes, not automatically as JSON. `core.*`
components are not application properties. The state is interest-filtered;
absence means not currently in the visible baseline, not proof of global deletion.

Publish a snapshot atomically as a replacement visible baseline; remove entities
absent from that baseline. Stage state messages arriving during assembly, then
apply those newer than its revision before declaring ready. Ignore old-socket
callbacks after reconnect. A delayed baseline must not roll back newer already
applied state. Equal-revision conflicting state requires recovery, not prediction.

Live messages use these semantics:

* `StateDelta`: `to_revision` is the instance boundary; each EntityState's
  `revision` is its entity revision. Merge only supplied transform/velocity/
  animation/presence fields. If `properties` is supplied, it is the complete
  custom property map at that boundary (remove omitted keys). Proto Transform
  is flat `position_x` etc., unlike snapshot JSON's nested transform; it does
  not carry scale. Preserve snapshot scale when applying transform deltas.
* Server `EntityCommand` with `operation:"update"`: replace just the selected
  custom component with `{encoding:"json",value:arguments}`. The key is
  `arguments.component_key` (legacy `key` fallback, then `entity.component`).
  `expected_revision` on this **server confirmation** is the resulting entity
  revision, and the optional `instance_revision` must be present for this
  contract. Preserve the full arguments object as component value.
* `spawn` confirmation: create the entity from the confirmed kind/transform
  arguments; arbitrary spawn arguments are not stored custom components.
  `delete` confirmation: remove the entity and retain a deletion boundary;
  its `expected_revision` is the instance revision, not an entity revision.

Track revision boundaries per entity **and per field/component**, plus snapshot
floor and deletion boundaries. Reliable component confirmations and latest-wins
deltas can overlap: a newer transform does not justify discarding a previously
unseen older component update. Conversely, never overwrite a field with older
data or resurrect it from before its deletion/snapshot floor. Byte/message
deduplication alone is insufficient for canonical reduction. Equal-boundary
different values are conflicts. The instance high-water mark is not a count of
visible changes; interest filtering/coalescing means revisions may skip. Do not
assume every global revision produces a visible message or blindly drop a delta
because `from_revision` differs from the high-water mark. Recover with a fresh
join/snapshot when state cannot be reconciled safely.

## Generic input and correlated results

Once ready with an owned/authorized existing entity, send `EntityCommand`:

```text
command_id       = fresh UUIDv7 (also Envelope.message_id)
entity_id        = provisioned entity UUID
expected_revision= exact canonical entity revision captured at submission
operation        = "input"
arguments        = Struct { "rule": "game.action", "intent": { ... } }
instance_revision= absent (server confirmation only)
```

`game.action` and the intent contents are placeholders for the operator's
contract, not built-in game rules. Keep a pending record of the original
ID/entity/revision/rule/intent. Do not send the desired authoritative component
as the outcome. Core obtains the registered component/service from trusted
operator configuration, computes and validates it, and commits only if the
observed entity state still matches. A client never calls the rule service with
the engine's signing secret. Normal permissions, durability and visibility apply.

There is no dedicated protobuf `InputResult` payload. Interpret existing messages:

| Outcome | Wire evidence |
| --- | --- |
| Accepted | Server EntityCommand with matching `command_id`, canonical `operation:"update"`, resulting entity `expected_revision` and present `instance_revision`; apply canonical data before notifying gameplay |
| Rejected | ErrorMessage whose `request_message_id` matches the sent envelope ID and represents a definite rejection, e.g. `INVALID_ARGUMENT` or `COMMAND_ID_CONFLICT` |
| Uncertain | Disconnect or local receipt timeout, or correlated error with `retryable:true`, `PERSISTENCE_UNAVAILABLE` or `REPLAY_WINDOW_EXPIRED` |

Do not compare server Envelope.message_id to your command ID; correlate the
payload fields above. A deduplicated confirmation may have an old revision:
acknowledge the pending operation without applying the mutation twice or rolling
state back. Uncertain means a command may already have committed. After recovery,
an explicit retry sends the **unchanged original command**, including its original
entity revision, ID and intent, in a new envelope with the connection's next
sequence and current send time. Keep envelope message ID equal to the command ID.
Changing semantics under the same command ID yields a conflict. Do not silently
turn an uncertain command into a fresh ID; that could duplicate an action.

Prediction/interpolation are client presentation policy, not wire requirements.
Keep predicted display separate from canonical state and reconcile to the
confirmed result. One outstanding input per controlled entity is a simple
starting policy; no game physics or update frequency is implied by this contract.

## Reconnect and resumption

On disconnect stop input, mark pending receipts uncertain and discard incomplete
snapshot assembly. Authenticate/refresh as needed, obtain a new ticket and run
the hello exchange on a new socket. A simple correct recovery option is always
to perform a fresh JoinInstance and complete snapshot; resume is optional.

To resume, retain the last issued token and exact last applied instance revision.
After new ServerHello send `ResumeSession` with that `resume_token`,
`last_applied_revision`, and `received_message_ids` (an empty list is supported).
The resume token is single-use and is **not authentication**. Store the rotated
token in ResumeAccepted. `replay_follows:true` means wait for state recovery;
current Core sends a snapshot. Do not declare ready from ResumeAccepted alone.
With `replay_follows:false`, reuse retained state only if it already matches the
offered current revision; otherwise require a fresh join. On ResyncRequired or
failed resume, use fresh join/snapshot. Never use old-socket frames in the new
connection's state. Bound reconnect attempts and surface terminal auth/version
failures rather than looping indefinitely.

The wire contract is language-neutral; interoperability in an arbitrary chosen
language still needs that client's implementation and validation. The included
TypeScript and Python checks do not claim that every language already has an SDK
or that arbitrary games have been tested.
