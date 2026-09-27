# @orbisync/client — TypeScript SDK

TypeScript SDK for OrbiSync realtime. The wire types are generated from `proto/orbisync/v1/realtime.proto` (source of truth, ADR-004) and the SDK uses `toBinary` / `fromBinary` from `@bufbuild/protobuf` for every WebSocket frame.

## Prerequisites

- Node 24.x (the SDK package's supported Node version)
- `buf` CLI 1.72+ and both local plugins required by the root `buf.gen.yaml`: `protoc-gen-es` and `protoc-gen-prost`, available on PATH. CI installs `@bufbuild/protoc-gen-es@2.13.0` and `protoc-gen-prost` 0.5.0.
- Rust workspace builds without it — SDK is optional

The SDK CI job currently selects Node 22 even though this package requires Node 24.x. Use Node 24.x for local SDK work; the documentation does not imply that the current CI configuration satisfies that requirement. CI runs are manual (`workflow_dispatch`), not automatic on each push or PR.

## Generate the wire code

Generated code is gitignored (ADR-004 — `.proto` is the source of truth, generated code is never committed). Generate after cloning and after changes to the proto source, before checking, testing, or packaging the SDK:

```bash
# from repo root
npm install --global @bufbuild/buf @bufbuild/protoc-gen-es@2.13.0
cargo install protoc-gen-prost --locked --version 0.5.0
buf generate
# or via the SDK script
npm run generate --prefix sdk/typescript
```

`buf.gen.yaml` writes to two places, and neither is `generated/typescript`:

| plugin | output |
|---|---|
| `protoc-gen-es` | `sdk/typescript/src/generated/orbisync/v1/realtime_pb.ts` |
| `protoc-gen-prost` | `generated/rust/` |

`.gitignore` covers both (`/generated/` and `sdk/typescript/src/generated/`). `client.ts` imports the TypeScript output relatively as `./generated/orbisync/v1/realtime_pb.js`, so when it is missing `tsc --noEmit` fails with `Cannot find module './generated/orbisync/v1/realtime_pb.js'`.

The reference Web app has a separate TypeScript-only generator using its local dependencies; its dev/build/check hooks run it automatically. It needs Node 22+ and no Rust plugin. See [the Web guide](../../apps/reference-web/README.md). The existence of an SDK generated file alone does not establish that it matches the current proto:

```bash
ls sdk/typescript/src/generated/orbisync/v1/realtime_pb.ts
```

## Develop

```bash
npm install --prefix sdk/typescript
npm run check --prefix sdk/typescript   # tsc --noEmit
cargo build -p orbisync-e2e-helper      # real-server fixture used by SDK tests, also built in CI
npm test --prefix sdk/typescript        # tsx --test src/**/*.test.ts (round-trip tests)
```

## Use from an app / example

For a local app depending on `file:../../sdk/typescript`, first generate the protocol code above, then build the SDK package entry point:

```bash
npm --prefix sdk/typescript ci
npm --prefix sdk/typescript run build
```

The app imports `dist/index.js`; generating protocol code or installing the app alone does not build that entry point. This applies to `apps/reference-console-web`.

```bash
# First obtain/build the SDK tarball as described below and place
# orbisync-client-0.1.0.tgz in examples/minimal-client-typescript/.
npm install --prefix examples/minimal-client-typescript
npm run check --prefix examples/minimal-client-typescript
npm run build --prefix examples/minimal-client-typescript
```

The minimal example is an executable Node 24 starter using the packaged `@orbisync/client`, `connectSession`, and `movementView`; it is not a fetch/WebSocket stub. Follow the [producer instructions](../../docs/consumer-kit/PRODUCING.md) to prepare the SDK tarball and the [example guide](../../examples/minimal-client-typescript/README.md) for environment variables, authentication, and server rule prerequisites before running it.

## Synchronized instance state

```ts
const connection = await client.connect();
const instance = await connection.join(instanceId);
await instance.ready({ timeoutMs: 10_000 });
const entities = instance.state.entities; // detached Map; revisions are bigint
instance.on("entityUpdated", () => render(instance.state));
```

`join()` acknowledges membership; `ready()` waits for a validated complete snapshot and buffered updates. Render `state` once after ready, since the initial snapshot may arrive before application handlers are installed. The SDK assembles chunks and maintains canonical state; remove application raw WebSocket listeners and reducers. A `snapshot` event now carries assembled bytes after state application, not one wire chunk.

Strict synchronization requires the negotiated `orbisync.state-sync.v1` server feature. Old servers reject ready with `UNSUPPORTED_SERVER`. Mutation sends require ready and otherwise throw `NOT_READY`. Listen to `syncStateChanged` to disable editing during recovery. Individual ready cancellation does not close the connection; leave/disconnect rejects pending waiters. See [frontend integration](../../docs/guides/frontend-integration.md#34-入室と初期状態の待機) for lifecycle and resource limits.

Spawn arguments are not persisted custom components. Applications such as the whiteboard confirm spawn, then send and confirm a component update using the resulting entity revision. `sendEntityCommand` returns its command ID and accepts an explicit UUIDv7 `commandId` for retry correlation; it does not return a persistence promise.

Revisions are uint64 `bigint`; numeric inputs must be safe integers. Struct custom arguments accept finite numbers, with integral values limited to the safe integer range. Encode larger custom integers as strings in new requests. Existing Core component bytes containing unsafe JSON numbers are exposed through `{ encoding: "base64", value: "..." }`, preserving their original bytes. They are never rounded to a number and converted back afterward. An old stored dedup receipt without `instance_revision` produces `UNSUPPORTED_SERVER`, settles pending synchronization, and is neither re-executed nor assigned today's revision.

State application compares snapshot floors, entity lifetimes, and each field/component separately. Full properties maps also advance an absence boundary, so an old component cannot reappear after a newer empty map. The largest observed instance revision alone never discards updates for another entity.

Default bounds apply in aggregate within each retained resource: snapshot assembly 16 MiB/1024 chunks/2 IDs; buffered notifications 4 MiB/2048 frames; encoded outgoing queues 4 MiB across both lanes; canonical state 32 MiB (`sync.maxStateBytes`, conservative UTF-16/container accounting). Individual incoming frames and outgoing encoded messages are limited to 64 KiB. There are at most 1024 ready waiters, 1024 listeners, 4096 tombstones, 65536 entities/presences, and 1030 field stamps per entity. The latest complete snapshot and one canonical baseline are additionally retained for duplicate validation; transactional application temporarily holds an old and staged state. These bounded copies are distinct from application-owned `state` copies. Disconnect/leave clears queued sends and pending assembly; explicit close also releases canonical state and listeners. Resource failures are typed and synchronization recovery remains finite.

## Why the round-trip test matters

`src/client.roundtrip.test.ts` encodes an `Envelope` with `encodeEnvelope` and decodes it with `decodeEnvelope` and asserts `messageId` / `sequence` (non-zero) / `payload` case and concrete fields (`positionX` etc.) plus `bytes.length > 0`. With the old stub (`return new Uint8Array()`) this test is red — tsc alone was green even then, so the test is the proof that the SDK actually speaks protobuf.


Strict synchronization recovery is bounded to five reconnect attempts. Exhaustion
emits `syncStateChanged` with `failed` and rejects pending/future `ready()` with
`SyncError.code === "RECOVERY_EXHAUSTED"`. The connection releases its listeners,
timers and pending work; a new connection is required to retry. Explicit `leave()`
closes the lifecycle and never schedules recovery.

## Server-authoritative inputs

`instance.sendInput({ entityId, rule, intent, expectedRevision?, commandId?, timeoutMs? })`
returns `{ request, result }`. Await `result` for `accepted`, `rejected`, or
`uncertain`. Acceptance follows canonical state sync; inspect `instance.state`
for the server-computed component. Preserve the returned `request` unchanged for
an explicit retry after uncertainty, including its original revision and ID.
See [the trusted server rule and movement example](../../examples/server-input/README.md).
Stock servers also load operator-provisioned external HTTPS JSON rules without
Rust recompilation. The same `sendInput` API consumes their rule name and intent
schema; see [external rule deployment](../../docs/consumer-kit/EXTERNAL-RULES.md).
# Movement display helpers

`PredictedInput<T>` provides immediate local display prediction and reconciliation
over `sendInput`, with one outstanding input and explicit uncertain-request retry.
`RemoteInterpolator<T>` interpolates bounded authoritative samples with a supplied
application policy. Neither helper writes canonical `instance.state`.
See [the runnable movement example and limits](../../examples/server-input/README.md#predicted-movement-and-remote-interpolation).

# Runnable connection starter

See [minimal-client-typescript](../../examples/minimal-client-typescript/README.md)
for auth discovery, connection/join, canonical state, input prediction, reconnect,
and cleanup using these SDK APIs, with a real-wire local fixture.
