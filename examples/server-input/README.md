# Server-authoritative intent

The trusted server composition registers an application rule, scoped to a world:

```rust
.with_input_rule(world_id, "example.move", Arc::new(Movement))
```

[`movement.rs`](movement.rs) implements the public `orbisync_server::input::InputRule`
trait. It reads canonical `example.position.x`, validates a requested `dx`, and
computes the next position. Client-supplied `x` is ignored. This is an application
component example, not Core transform/physics policy. The integration fixture
compiles this exact example and exercises it over real WebSockets.

```ts
await instance.ready();
const submission = instance.sendInput({
  entityId,
  rule: "example.move",
  intent: { dx: 1 },
});
const result = await submission.result;
if (result.status === "accepted") {
  console.log(instance.state.entities.get(entityId)?.properties["example.position"]);
}
// If uncertain, preserve submission.request (including its original revision and
// command ID). After reconnect and ready(), explicitly retry that same request:
// const retry = instance.sendInput(submission.request);
```

The wire request uses the existing reliable `EntityCommand`: `operation: "input"`,
`arguments: { rule, intent }`, entity ID, expected entity revision, and UUIDv7
command ID. The server returns its computed canonical `update` with that command
ID and committed revisions; peers receive the same update through existing
interest-filtered sync. A correlated Error is a rejection, except retryable or
persistence/replay-expiry errors, which the SDK reports as uncertain. Disconnect
and receipt timeout are also uncertain, never proof of rejection. Local invalid
arguments or enqueue failure throw synchronously. The timeout is caller-configurable.
Older servers reject the unknown operation; no new protocol schema is needed.

Local rules are trusted synchronous Rust code installed at composition time.
For source-independent deployment, the stock server also supports
[external HTTPS JSON rules](../../docs/consumer-kit/EXTERNAL-RULES.md) through
operator startup configuration. Neither kind is uploaded by clients.
Each rule declares its controlled component key. The adapter
sets that key on the computed output and refuses direct client updates to it in
that world; unrelated component updates, spawn/delete, and transform APIs retain
their existing authorization. Rule registration does not replace entity lifecycle
policy: applications that require restricted spawning/deletion must use existing
authorization/precommit controls. Registration should remain stable across retries
and restarts, like application policy generally.

The minimal contract computes one component mutation on the addressed entity.
Rules see authenticated requester, world, instance, server time, and an immutable
entity snapshot. They must be pure and bounded, without I/O or external side
effects: concurrent state changes may discard their output. Existing permissions,
component validation, update precommit hooks, exact entity-state guards, commit,
persistence, deduplication, and delivery are reused. No multi-entity transaction
or runtime plugin loader is included.

Retries keep the *original intent fingerprint*, not the computed update's
fingerprint, and use the existing retention window. Do not retry an expired
receipt blindly: `REPLAY_WINDOW_EXPIRED` is uncertain and requires application
reconciliation. The SDK does not automatically resend inputs. A receipt timeout
can occur while queued; the original request may still complete.

## Predicted movement and remote interpolation

[`predicted-movement.ts`](predicted-movement.ts) connects the SDK's generic
`PredictedInput<T>` and `RemoteInterpolator<T>` to the same Rust rule. After joining
an instance with visible local and remote entities:

```ts
import { movementView } from "./examples/server-input/predicted-movement.js";
const view = await movementView(instance, localEntityId, remoteEntityId);
const receipt = view.step(1);
console.log(view.frame()); // predicted x changes immediately; authoritative x does not
const result = await receipt;
console.log(view.frame()); // server-committed position replaces prediction
// Call view.frame() from your render loop; call view.dispose() on teardown.
```

Use one controller per controlled entity. This deliberately supports **one
outstanding input**, without a local input queue: wait for its receipt before
submitting the next step. Multiple commands sent with the same `expectedRevision`
are not a continuous movement pipeline. Applications choose their step cadence;
these helpers do not implement physics, velocity integration, collisions or
rollback. Server movement judgment remains the replaceable `Movement` InputRule.

Prediction uses detached values and never changes `instance.state`. The
`authoritative` and `display` getters distinguish canonical truth from display.
An authoritative revision advance immediately replaces the prediction, even before
the correlated promise resolves. Rejection removes prediction. Missing entities
return undefined. Only submit while sync is ready.

Timeout/disconnect disables prediction and keeps `pendingRequest`, blocking new
steps. After `await instance.ready()`, display follows the refreshed canonical
state. The application can explicitly call `view.local.retry()` to resend exactly
the original request, or `view.local.useCanonical()` to stop tracking it without
resending. Neither option silently creates a new ID for an uncertain intent.
`REPLAY_WINDOW_EXPIRED` should be reconciled with canonical state, not repeatedly
retried. Abandoning tracking does not cancel a queued/in-flight command: a late
commit may still appear through sync. Reconnect alone cannot prove rejection.

Remote samples use authoritative instance revisions and local monotonic receive
times. The bounded buffer interpolates at `now - delayMs`, rejects stale samples,
and holds endpoints instead of extrapolating. The example resets history on
snapshot, reconnect, deletion and interest loss. The interpolation delay is a
display setting, not a latency guarantee. It does not estimate server clock skew.

Run the small functional checks from the repository root (Node dependencies and
generated protocol types are local prerequisites):

```sh
npm ci --prefix sdk/typescript
buf generate
npm run check --prefix sdk/typescript
node --import ./sdk/typescript/node_modules/tsx/dist/loader.mjs --test sdk/typescript/src/motion.test.ts sdk/typescript/src/client.input.test.ts
cargo test -p orbisync-integration-tests --test entity_w16 server_input -- --nocapture
```

The TCP fixture exercises the compiled Rust rule, computed movement, canonical
peer delivery, deduplicated retry and rejection. Deterministic SDK tests cover
prediction, correction, uncertainty/reconnect and interpolation using actual sync
envelopes; they do not claim a browser or full SDK-to-server movement session.
