# @orbisync/client

Built ESM SDK for canonical realtime state and server-authoritative input.
Tested on Node 24.x with native WebSocket. Install the supplied tarball:

```sh
npm install ./orbisync-client-0.1.0.tgz
```

```ts
import { connectSession, movementView, PredictedInput } from "@orbisync/client";
```

For source-checkout readers, the guides are available in
[docs/consumer-kit](../../docs/consumer-kit/README.md). The `dist/` links below
refer to the built SDK package and become available after the documented build.

JavaScript, declarations and generated protobuf runtime are included. Consumers
need no engine checkout, Rust or protocol generation. There are no install hooks.
Read [the integration guide](dist/INTEGRATION.md) and
[LLM handoff](dist/LLM-INTEGRATION.md) for lifecycle, data/input contracts,
operator configuration, browser applicability and new/existing game integration.
The guide's starter commands apply when you receive the complete consumer kit.

This is the TypeScript/JavaScript SDK, not a claim of SDK availability in every
language. The engine contracts are language-neutral. Other-language consumers
can use [the wire guide](dist/CLIENT-WIRE.md) and bundled `dist/protocol/`
protobuf/OpenAPI schemas without engine source. Python is only an external
rule-service example; games and services are not restricted to that language.

The operator supplies an endpoint, enabled authentication, instance and owned
entity IDs, and a provisioned rule contract. Arbitrary trusted server rules cannot
be uploaded/registered by players. Operators can deploy language-neutral HTTPS
rules through the stock server's startup manifest; see
[external rule deployment and Python example](dist/EXTERNAL-RULES.md).
The optional movementView uses the example.move/example.position contract only.

Modern browser apps can bundle the SDK; it is not a script-tag bundle. The CLI
starter is Node-only. Node24 fixture evidence does not establish browser,
production auth, persistence or performance acceptance. Licenses are in dist.

```bash
cargo build -p orbisync-e2e-helper       # required by the reconnect E2E test
npm install --prefix sdk/typescript
npm run check --prefix sdk/typescript   # tsc --noEmit
npm test --prefix sdk/typescript        # tsx --test src/**/*.test.ts (round-trip tests)
```

Run these commands from the repository root. The test runner deliberately does
not compile the Rust helper inside its 15-second startup timeout; this keeps a
cold build from timing out and leaving a child process behind.

## Use from an app / example

```bash
npm ci --prefix apps/reference-web
npm run dev --prefix apps/reference-web
```

`apps/reference-web` imports this package through a local file dependency and
uses the real login, authenticated REST, realtime join, Entity command, and
resume paths. It does not have a stub fallback.

All publicly visible IDs use UUIDv7. Use `createUuidV7()` for Entity IDs and
application-generated IDs; do not use `crypto.randomUUID()`, which returns
UUIDv4. The SDK itself uses the same helper for envelope, command, and event
IDs.

Authenticated REST calls can share the SDK's in-memory token lifecycle without
exposing the access token:

```ts
await client.auth.login({ loginId, password });
const response = await client.fetch("/v1/worlds?limit=100");
```

`client.fetch()` accepts only same-origin paths beginning with `/`, performs
proactive refresh, and retries one 401 response after refresh when possible.

## Observe connection state

`OrbiSyncConnection` exposes an immutable, typed snapshot suitable for connection indicators and retry UI. Subscription immediately emits the current value by default and returns an unsubscribe function.

```ts
const connection = await client.connect();
const unsubscribe = connection.onConnectionStateChange((state) => {
  console.log(state.phase);                 // connected | reconnecting | resyncing | offline | closed
  console.log(state.reconnectAttempt);      // attempts already started
  console.log(state.lastDisconnect);        // close code/reason, or null
  console.log(state.rttMs);                 // latest heartbeat RTT, or null
  console.log(state.lastAppliedRevision);   // bigint
});

const current = connection.getConnectionState();
connection.requestReconnect(); // reuses the normal automatic resume path
unsubscribe();
```

Snapshots are also emitted when the reconnect attempt, RTT, or last-applied revision changes. `resumeToken` is intentionally absent from both the public type and runtime object. `lastAppliedRevision` is a `bigint`; convert it to a string before JSON serialization.

After `join()`, `instance.getJoinInfo()` returns the authenticated user and
presence IDs, resolved Entity permissions, initial revision, and nearby counts.
It deliberately omits the resume token. Successful reliable EntityCommand
results update the SDK's tracked entity revision, so a later update/delete can
normally omit `expectedRevision`.

## Track a confirmed EntityCommand

After `await instance.ready()`, the generic command API keeps one immutable
request (ID, JSON arguments and expected revision) for each explicit submission:

```ts
const tracked = instance.commands.submit({
  entityId,
  operation: "update",
  args: { component_key: "app.label", value: "hello" },
}, { timeoutMs: 10_000 });
const result = await tracked.result;
if (result.status === "unknown") {
  // Show an explicit retry action after the instance is ready again.
  const retryResult = await tracked.retry(); // same ID, payload and revision
}
console.log(tracked.request.commandId, tracked.state.status);
```

`committed` means a matching server receipt was applied to canonical state and
includes Entity and Instance revisions. `rejected` means the first attempt was
definitively rejected. A timeout, disconnect, send error or uncertain server
error produces `unknown`: it does **not** mean the operation was not executed.
A rejection after an unknown attempt remains unknown, since it cannot disprove
an earlier commit. A late matching receipt changes the handle's state to
`committed`, even after an attempt's Promise has already resolved as unknown.
Snapshots alone never prove that a particular command executed.

The SDK removes tracked unknown requests from automatic transport replay. Only
`tracked.retry()` resends the retained request, and it requires ready state.
Retries never allocate a replacement command ID or advance the saved revision.
Use `tracked.end()` to stop tracking, or leave/close the Instance to obtain
`ended`; neither operation cancels an operation already sent to the server.

`instance.commands.onChange(listener)` returns an unsubscribe function.
`pending`, `metrics` and `get(commandId)` inspect local unresolved tracking;
`get` is not a server-side result query. Completed results remain on the handle
held by the application. Each Instance retains at most 256 unresolved commands
and 4 MiB of requests (including unknown outcomes); submission throws
`SyncError("RESOURCE_LIMIT")` when either budget is exhausted. End tracking to
release capacity only after explaining the unknown outcome to the user.
Arguments must be plain JSON objects/arrays and are copied and deeply frozen;
mutable typed arrays and class instances are rejected. Existing
`sendEntityCommand`, `sendInput` and ownership helpers keep their existing APIs.

## Transfer entity ownership

Ownership transfer uses the same reliable queue and `command_id` deduplication
as other entity commands. The target must currently be joined to the same
instance; the server validates ownership, permissions, membership and the
expected revision.

```ts
instance.transferEntityOwnership({
  entityId: "0192d43d-a18a-7fed-8123-0123456789ab",
  newOwnerId: "0192d43d-a18a-7fed-8123-1123456789ab",
  expectedRevision: 4n, // omit to use the SDK's tracked entity revision
});
```

Like `sendEntityCommand`, this method throws `ReliableQueueOverflowError`
synchronously if the reliable queue is full.

## Why the round-trip test matters

`src/client.roundtrip.test.ts` encodes an `Envelope` with `encodeEnvelope` and decodes it with `decodeEnvelope` and asserts `messageId` / `sequence` (non-zero) / `payload` case and concrete fields (`positionX` etc.) plus `bytes.length > 0`. With the old stub (`return new Uint8Array()`) this test is red — tsc alone was green even then, so the test is the proof that the SDK actually speaks protobuf.
