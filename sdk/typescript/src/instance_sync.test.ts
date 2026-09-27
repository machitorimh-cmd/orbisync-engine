import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { InstanceSync } from "./instance_sync.js";
import { SyncError } from "./sync_state.js";

const bytes = (revision = 100n) => new TextEncoder().encode(`{"format":"orbisync.snapshot.v1","revision":${revision},"instance":{"instance_id":"room"},"entities":[],"users":[]}`);
const chunk = (data: Uint8Array, index = 0, count = 1, revision = 100n, snapshotId = "s") => create(EnvelopeSchema, {
  instanceId: "room", payload: { case: "snapshot", value: { snapshotId, chunkIndex: index, chunkCount: count, instanceRevision: revision, data } },
});
function setup(options = {}) {
  const events: string[] = [], errors: SyncError[] = [];
  const sync = new InstanceSync("room", event => events.push(event), error => errors.push(error), options);
  sync.begin(1, true);
  return { sync, events, errors };
}
describe("instance synchronization lifecycle", () => {
  it("ignores variable snapshot metadata at an identical committed baseline", async () => {
    const { sync, errors } = setup();
    const first = JSON.parse(new TextDecoder().decode(bytes()));
    first.server_time_unix_ms = 10;
    sync.receive(chunk(new TextEncoder().encode(JSON.stringify(first))));
    await sync.ready();
    first.server_time_unix_ms = 20;
    sync.receive(chunk(new TextEncoder().encode(JSON.stringify(first)), 0, 1, 100n, "same-baseline-new-time"));
    await sync.ready();
    assert.equal(errors.length, 0);
    sync.stop(new SyncError("CLOSED"), "closed");
  });
  it("fails a restored legacy receipt without reapplying it or retrying synchronization", async () => {
    const { sync, errors } = setup();
    sync.receive(chunk(bytes()));
    await sync.ready();
    sync.receive(create(EnvelopeSchema, { instanceId: "room", payload: { case: "entityCommand", value: {
      entityId: "legacy", operation: "spawn", expectedRevision: 1n, arguments: { kind: "object" },
    } } }));
    assert.equal(sync.status, "failed");
    assert.equal(errors[0]?.code, "UNSUPPORTED_SERVER");
    assert.equal(sync.state.entities.size, 0);
    await assert.rejects(sync.ready(), { code: "UNSUPPORTED_SERVER" });
    sync.stop(new SyncError("CLOSED"), "closed");
  });
  it("publishes only complete snapshots and atomically includes updates during assembly", async () => {
    const { sync, events, errors } = setup();
    const data = bytes();
    const ready = sync.ready();
    sync.receive(chunk(data.slice(0, 20), 0, 2));
    sync.receive(create(EnvelopeSchema, { instanceId: "room", payload: { case: "entityCommand", value: { entityId: "a", operation: "spawn", expectedRevision: 1n, instanceRevision: 101n, arguments: { kind: "note" } } } }));
    assert.equal(sync.state.entities.size, 0);
    assert.equal(events.includes("snapshot"), false);
    sync.receive(chunk(data.slice(20), 1, 2));
    await ready;
    assert.equal(sync.status, "ready");
    assert.equal(sync.state.revision, 101n);
    assert.equal(sync.state.entities.get("a")?.revision, 1n);
    assert.equal(events.filter(x => x === "snapshot").length, 1);
    assert.equal(errors.length, 0);
    sync.stop(new SyncError("CLOSED"), "closed");
  });
  it("settles individual abort without cancelling another ready waiter", async () => {
    const { sync } = setup();
    const controller = new AbortController();
    const cancelled = assert.rejects(sync.ready({ signal: controller.signal }), { code: "ABORTED" });
    const other = sync.ready();
    controller.abort();
    sync.receive(chunk(bytes()));
    await Promise.all([cancelled, other]);
    assert.equal(sync.status, "ready");
    sync.stop(new SyncError("CLOSED"), "closed");
    await assert.rejects(sync.ready(), { code: "CLOSED" });
  });
  it("rejects conflicting completed chunks and preserves committed state during recovery", async () => {
    const { sync, errors } = setup();
    sync.receive(chunk(bytes()));
    await sync.ready();
    const corrupt = bytes(); corrupt[10] ^= 1;
    sync.receive(chunk(corrupt));
    assert.equal(sync.status, "recovering");
    assert.equal(sync.state.revision, 100n);
    assert.equal(errors[0]?.code, "REVISION_CONFLICT");
    sync.stop(new SyncError("CLOSED"), "closed");
  });
  it("bounds pending updates and settles all waiters on overflow", async () => {
    const { sync, errors } = setup({ maxPendingMessages: 1 });
    const wait = assert.rejects(sync.ready(), { code: "RESOURCE_LIMIT" });
    const delta = create(EnvelopeSchema, { instanceId: "room", payload: { case: "stateDelta", value: { fromRevision: 100n, toRevision: 101n } } });
    sync.receive(delta); sync.receive(delta);
    await wait;
    assert.equal(errors.length, 1);
    assert.equal(sync.state.revision, 0n);
    sync.stop(new SyncError("CLOSED"), "closed");
  });
  it("keeps reconnect waiters through generation setup, rejects unsupported server explicitly", async () => {
    const { sync } = setup();
    sync.stop(new SyncError("DISCONNECTED"));
    const waiting = sync.ready();
    sync.begin(2, true); sync.receive(chunk(bytes()));
    await waiting;
    sync.begin(3, false);
    await assert.rejects(sync.ready(), { code: "UNSUPPORTED_SERVER" });
  });
});

 it("does not emit old snapshot events after a ready handler leaves", () => {
   const events: string[] = [];
   const sync = new InstanceSync("room", (event, payload) => {
     events.push(event);
     if (event === "syncStateChanged" && payload === "ready") sync.stop(new SyncError("CLOSED"), "closed");
   }, () => assert.fail("must not recover after leave"));
   sync.begin(1, true);
   sync.receive(chunk(bytes()));
   assert.equal(sync.status, "closed");
   assert.equal(events.includes("snapshot"), false);
 });
 it("does not arm a timeout after a syncing handler leaves", () => {
   const sync = new InstanceSync("room", (event, payload) => {
     if (event === "syncStateChanged" && payload === "syncing") sync.stop(new SyncError("CLOSED"), "closed");
   }, () => assert.fail("must not recover after leave"));
   sync.begin(1, true);
   assert.equal(sync.status, "closed");
 });

it("a delayed snapshot above the floor cannot roll back already committed live state", async () => {
  const { sync } = setup();sync.receive(chunk(bytes()));
  sync.receive(create(EnvelopeSchema,{instanceId:"room",payload:{case:"entityCommand",value:{entityId:"a",operation:"spawn",instanceRevision:110n,expectedRevision:1n,arguments:{kind:"note"}}}}));
  sync.receive(chunk(bytes(105n),0,1,105n,"delayed"));
  await sync.ready();assert.equal(sync.state.revision,110n);assert.equal(sync.state.entities.has("a"),true);
  sync.stop(new SyncError("CLOSED"),"closed");
});
