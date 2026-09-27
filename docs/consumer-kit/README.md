# OrbiSync consumer kit

This kit connects a new or existing game to an operator-provisioned OrbiSync
endpoint. It includes an installable SDK tarball (ESM JavaScript, TypeScript
declarations and generated protobuf runtime), a runnable TypeScript starter,
and this integration contract. No engine checkout, Rust, Buf, generated-code
tools, internal tests or engine source is required by consumers.

Both the client protocol and external rule HTTP contract are language-neutral.
The supplied SDK/starter is TypeScript/JavaScript; other languages do not thereby
have an SDK. For an implementation in another language, use
[CLIENT-WIRE.md](CLIENT-WIRE.md) and the bundled public protobuf/OpenAPI files in
`protocol/`. That path requires your language's protocol libraries/bindings, not
engine source. The Python rule service is an example of the HTTP contract only;
neither games nor rule implementations are limited to Python or this example.

For the supplied TypeScript starter, use Node **24.x** with native WebSocket enabled. Dependency installation needs
access to the npm registry; this kit is not an offline dependency mirror.

## Run the starter

Extract the archive, then:

```sh
cd starter
npm ci
npm run check
npm run build
```

Copy `connection.env.example` to `.env` and fill in operator-provided values.
Never commit that file. Keep passwords/tokens in a secret manager or process
environment where possible. Start with either environment variables already
set (`npm start`) or Node's local environment-file loader:

```sh
node --env-file=.env dist/main.js
```

Commands: `1`, `0`, `-1` send movement intent; `frame` shows authoritative,
predicted and interpolated remote position; `retry` explicitly retries an
uncertain request; `canonical` stops tracking it; `quit`, EOF or Ctrl+C cleans up.
Wait for each receipt. This CLI requires an existing visible owned entity and
the exact example movement contract described below. It creates no worlds or
entities. `REMOTE_ENTITY_ID` defaults to `ENTITY_ID` for a single-entity demo.

For an existing application, copy the tarball to the application and run:

```sh
npm install ./orbisync-client-0.1.0.tgz
```

Use public package imports only; declarations are included. There are no consumer
install hooks, compilation or protocol-generation steps in the SDK package.

## Operator handoff (required before gameplay)

The operator supplies these separately from frontend code:

| Value | Meaning |
| --- | --- |
| `SERVER_URL` | HTTP(S) API base URL without embedded credentials; HTTPS for deployed browser apps |
| `INSTANCE_ID` | Running world instance UUID, **not** the world definition ID |
| `AUTH_METHOD` | Explicit enabled method: `local`, `guest`, `name_only`, `external` |
| `LOGIN_ID`, `PASSWORD` | Local mode credentials supplied securely |
| `DISPLAY_NAME` | Name-only mode display name |
| `EXTERNAL_TOKEN` | External mode token from the configured issuer |
| `ENTITY_ID` | Existing entity UUID owned by this identity and visible in this instance |
| `REMOTE_ENTITY_ID` | Optional visible peer entity UUID |
| Rule contract | Provisioned rule name, intent schema, canonical component schema, limits and ownership policy |

Guest/name-only auth creates an identity. Provision an entity for that identity
through the application's authorized flow before constructing its movement view;
the static CLI is best used with an already provisioned local identity. Discovery
rejects a disabled method without silently changing identity. Server setup must
provide HTTP auth/tickets, `/ws` with `orbisync.v1.protobuf`, and canonical state
sync. The SDK handles `realtime_ticket`, ticket expiry, heartbeat and reconnect.

**Server rule boundary:** stock engine operators can provision external HTTPS
JSON rules with a world/rule/component manifest. Read [EXTERNAL-RULES.md](EXTERNAL-RULES.md)
for registration commands, public auth/world/instance/entity setup interfaces,
the language-neutral contract, and the standalone Python rule service. Consumers
can implement and deploy application rules without engine source or Rust builds.
The operator installs the manifest and shared secret; players cannot register
rules. Local Rust rules remain supported. Registration does not provision
entities or replace spawn/delete permissions.

## Integrate and render

```ts
import { connectSession, movementView } from "@orbisync/client";

// All IDs, endpoint and authentication are injected by your application.
const session = await connectSession({
  baseUrl, instanceId, authentication,
  onState: instance => render(instance.state, instance.syncStatus),
});
const view = await movementView(session.instance, localEntityId, remoteEntityId);
// In your input handler, while ready and with no pendingRequest:
const receipt = view.step(1);
renderPosition(view.frame()); // prediction changes immediately
const result = await receipt;
renderPosition(view.frame()); // canonical acceptance/correction
// Sample view.frame() each render frame for remote interpolation.
// At scene/application shutdown, in a finally block:
view.dispose();
await session.leave();
```

`connectSession` discovers methods, authenticates, connects, joins, waits for
ready state and subscribes `onState` to snapshot/entity/sync/error notifications.
Keep that callback short and nonthrowing. Setup failure disconnects the opened
connection. `leave()` detaches listeners and closes the joined session; dispose
game views first. Handle setup failure and terminal lifecycle errors in your UI.

`instance.state` contains `revision: bigint`, `entities: ReadonlyMap<string,
SyncedEntity>`, `presences` and `metadata`. An entity has `entityId`, `revision`
and `instanceRevision` (bigints), `properties`, and optional transform/velocity/
animation/presence fields. Read canonical state without mutating it. Use string
conversion for revision display; JSON.stringify cannot directly encode bigints.
Handle absent/deleted/hidden entities and reset render state on snapshots.

`syncStatus` is `syncing`, `ready`, `reconnecting`, `recovering`, `failed` or
`closed`. Disable input outside `ready`. Await `instance.ready()` for recovery;
it can reject. The SDK recovers the same instance using resume or a fresh join;
terminal failure requires an application decision. It does not retry forever.

## Movement and custom game contracts

`movementView` is an optional application example, not a general physics engine.
It uses the fixed provisioned rule `example.move` with intent `{ dx: number }`.
The rule accepts finite `dx` between -1 and 1 inclusive, reads canonical `x`
(zero when absent), and computes `x + dx`. The synchronized component is
`entity.properties["example.position"]` with a `value` object containing numeric
`x` (and the component key in the rule output). Out-of-range input is rejected.
The display predictor uses the same addition; remote samples are interpolated
and reset on snapshot/lifecycle changes. Do not send a desired canonical `x`.

For another **already provisioned** contract, use the generic exported helper:

```ts
import { PredictedInput } from "@orbisync/client";
const controller = new PredictedInput(instance, entityId, contract.rule,
  entity => readGameState(entity),
  (state, intent) => predictGameState(state, intent));
const result = await controller.submit(intent);
// controller.authoritative / controller.display / controller.pendingRequest
controller.dispose();
```

Or use `instance.sendInput({ entityId, rule, intent, expectedRevision?,
commandId?, timeoutMs? })`, which returns `{ request, result }`. `result` is a
promise. The SDK creates a UUIDv7 command ID and captures the canonical entity
revision when omitted. Preserve the returned request unchanged for retry.
Rule intent must follow the operator's schema; rule names are not arbitrary
client-side registrations. See the installed declarations for exact types.

Inputs use reliable EntityCommand operation `input`; the server checks identity,
ownership, revision and trusted rule validation, then broadcasts its canonical
update. Only one outstanding request per prediction controller is allowed.
`accepted` means a correlated canonical update was reflected; `rejected` means
a definite rejection; `uncertain` means the outcome is unknown (including
disconnect, timeout and selected persistence/replay errors). Inspect the result
code. Local validation/enqueue errors can throw synchronously.

An uncertain request may commit later. Reconnect never automatically resends
intent. After ready, `controller.retry()` explicitly resends the original
ID/revision/intent without prediction. `controller.useCanonical()` abandons
tracking without sending or cancelling the server command. Reconcile expired
replay evidence using canonical state/operator policy; do not blindly create a
new command ID or assume a timeout rolled back the action.

## Browser applicability and evidence limits

The SDK and reusable helpers use browser APIs (fetch, WebSocket, crypto,
performance and structuredClone) and ESM imports. A modern browser application
can bundle `@orbisync/client` with its existing bundler; this is not a self-contained
script-tag bundle. The CLI itself uses Node readline/process and is Node-only.
Inject endpoint/config into browser code through your application's configuration
and auth UI. Never embed privileged credentials in a browser bundle. The operator
must permit the browser origin/CORS and compatible HTTPS/WSS access.

This kit is functionally verified on Node 24 against a small real realtime
server fixture, with fixture authentication and non-durable stores. It does not
establish production auth, browser deployment, persistence or load acceptance.
No older Node compatibility or performance claim is made.
