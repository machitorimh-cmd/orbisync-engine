import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { OrbiSyncInstance, PredictedInput, RemoteInterpolator, SyncError, type SyncedEntity } from "./client.js";

const read = (entity: SyncedEntity): number => (entity.properties["example.position"] as { value: { x: number } }).value.x;
function fixture() {
  const sent: Uint8Array[] = [];
  const instance = new OrbiSyncInstance({ readyState: 1, bufferedAmount: 0,
    send: (bytes: Uint8Array) => sent.push(bytes) } as unknown as WebSocket, "room", () => 1);
  function snapshot(generation = 1) {
    instance._beginSync(generation, true);
    instance._dispatch(create(EnvelopeSchema, { instanceId: "room", payload: { case: "snapshot", value: {
      snapshotId: `s${generation}`, chunkCount: 1, instanceRevision: 100n,
      data: new TextEncoder().encode('{"format":"orbisync.snapshot.v1","revision":100,"instance":{"instance_id":"room"},"entities":[],"users":[]}'),
    } } }));
  }
  function update(x: number, revision: bigint, instanceRevision: bigint, commandId = "") {
    instance._dispatch(create(EnvelopeSchema, { instanceId: "room", payload: { case: "entityCommand", value: {
      entityId: "a", operation: "update", expectedRevision: revision, instanceRevision, commandId,
      arguments: { component_key: "example.position", x },
    } } }));
  }
  snapshot(); update(0, 1n, 101n);
  const local = new PredictedInput(instance, "a", "example.move", read, (x, intent) => x + Number(intent.dx));
  const close = () => { local.dispose(); instance._stopSync(new SyncError("CLOSED"), "closed"); };
  return { instance, local, sent, update, snapshot, close };
}

it("predicts immediately, corrects before receipt callbacks, and serializes revision-based input", async () => {
  const f = fixture();
  try {
    const result = f.local.submit({ dx: 1 });
    assert.equal(f.local.display, 1); assert.equal(f.local.authoritative, 0);
    assert.throws(() => f.local.submit({ dx: 1 }), /pending/);
    const id = f.local.pendingRequest!.commandId;
    f.update(0.75, 2n, 102n, id);
    assert.equal(f.local.display, 0.75); // no double prediction while promise is pending
    assert.equal((await result).status, "accepted");
    assert.equal(f.local.pendingRequest, undefined);
    f.update(999, 1n, 101n); // stale canonical sample ignored by sync
    assert.equal(f.local.display, 0.75);
    const next = f.local.submit({ dx: 1 });
    assert.equal(f.local.pendingRequest!.expectedRevision, 2n);
    assert.equal(f.local.display, 1.75);
    f.instance._dispatch(create(EnvelopeSchema, { payload: { case: "error", value: {
      code: "INVALID_ARGUMENT", requestMessageId: f.local.pendingRequest!.commandId,
    } } }));
    assert.equal((await next).status, "rejected");
    assert.equal(f.local.display, 0.75); assert.equal(f.local.authoritative, 0.75);
  } finally { f.close(); }
});

it("disconnect and reconnect show canonical state, retaining only an explicit same-ID retry", async () => {
  const f = fixture();
  try {
    const result = f.local.submit({ dx: 1 });
    const request = f.local.pendingRequest!;
    f.instance._stopSync(new SyncError("DISCONNECTED"));
    assert.equal(f.local.display, 0);
    assert.equal((await result).status, "uncertain");
    f.snapshot(2); f.update(1, 2n, 102n);
    assert.equal(f.local.display, 1); assert.equal(f.sent.length, 1);
    assert.deepEqual(f.local.pendingRequest, request);
    assert.throws(() => f.local.submit({ dx: 1 }), /pending/);
    const retry = f.local.retry();
    assert.deepEqual(f.local.pendingRequest, request);
    assert.equal(f.local.display, 1);
    f.update(1, 2n, 102n, request.commandId);
    assert.equal((await retry).status, "accepted");
    assert.equal(f.local.display, 1);
  } finally { f.close(); }
});

it("timeout drops prediction and canonical adoption does not resend an uncertain intent", async () => {
  const f = fixture();
  try {
    const result = f.local.submit({ dx: 1 }, 1);
    await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal((await result).status, "uncertain");
    assert.equal(f.local.display, 0);
    f.local.useCanonical();
    assert.equal(f.local.pendingRequest, undefined); assert.equal(f.sent.length, 1);
    f.update(1, 2n, 102n); // late original commit remains canonical truth
    assert.equal(f.local.display, 1);
  } finally { f.close(); }
});

it("interpolates authoritative samples, rejects stale revisions, holds endpoints and resets", () => {
  const f = fixture();
  try {
    const remote = new RemoteInterpolator<number>((a, b, t) => a + (b - a) * t, 10);
    remote.push(f.instance.state.entities.get("a")!.instanceRevision, 0, f.local.authoritative!);
    f.update(1, 2n, 102n);
    remote.push(f.instance.state.entities.get("a")!.instanceRevision, 20, f.local.authoritative!);
    assert.equal(remote.sample(20), 0.5);
    assert.equal(remote.push(101n, 30, 999), false);
    assert.equal(remote.sample(0), 0); assert.equal(remote.sample(100), 1);
    remote.reset(); assert.equal(remote.sample(100), undefined);
    remote.push(200n, 100, 4); assert.equal(remote.sample(100), 4);
  } finally { f.close(); }
});
