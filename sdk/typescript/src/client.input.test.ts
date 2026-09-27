import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { OrbiSyncInstance, decodeEnvelope, SyncError } from "./client.js";

function fixture() {
  const sent: Uint8Array[] = [];
  const ws = { bufferedAmount: 0, readyState: 1, send: (bytes: Uint8Array) => sent.push(bytes) };
  const instance = new OrbiSyncInstance(ws as unknown as WebSocket, "room", () => 1);
  instance._beginSync(1, true);
  instance._dispatch(create(EnvelopeSchema, { instanceId: "room", payload: { case: "snapshot", value: {
    snapshotId: "s", chunkCount: 1, instanceRevision: 100n,
    data: new TextEncoder().encode('{"format":"orbisync.snapshot.v1","revision":100,"instance":{"instance_id":"room"},"entities":[],"users":[]}'),
  } } }));
  return { instance, sent };
}
it("sends only intent, correlates receipt after canonical sync, and preserves retry revision", async () => {
  const { instance, sent } = fixture();
  const submission = instance.sendInput({ entityId: "a", rule: "example.move", intent: { dx: 1 }, expectedRevision: 1n });
  const wire = decodeEnvelope(sent[0]);
  assert.equal(wire.messageId, submission.request.commandId);
  assert.equal(wire.payload.case, "entityCommand");
  if (wire.payload.case !== "entityCommand") throw new Error("wrong payload");
  assert.equal(wire.payload.value.operation, "input");
  assert.deepEqual(wire.payload.value.arguments, { rule: "example.move", intent: { dx: 1 } });
  const receipt = create(EnvelopeSchema, { instanceId: "room", payload: { case: "entityCommand", value: {
    commandId: submission.request.commandId, entityId: "a", operation: "update",
    expectedRevision: 2n, instanceRevision: 101n, arguments: { component_key: "example.position", x: 1 },
  } } });
  instance._dispatch(receipt);
  assert.equal((await submission.result).status, "accepted");
  assert.equal(instance.state.entities.get("a")?.revision, 2n);
  const canonical = instance.state.entities.get("a")?.properties["example.position"];
  assert.deepEqual(canonical, { encoding: "json", value: { component_key: "example.position", x: 1 } });
  const retry = instance.sendInput(submission.request);
  instance._dispatch(receipt);
  assert.equal((await retry.result).status, "accepted");
  assert.deepEqual(instance.state.entities.get("a")?.properties["example.position"], canonical);
  const retryWire = decodeEnvelope(sent[1]);
  if (retryWire.payload.case !== "entityCommand") throw new Error("wrong payload");
  assert.equal(retryWire.payload.value.expectedRevision, 1n);
  instance._stopSync(new SyncError("CLOSED"), "closed");
});
it("correlates rejection and uncertain durability errors without changing canonical state", async () => {
  const { instance } = fixture();
  for (const [code, retryable, status] of [["INVALID_ARGUMENT", false, "rejected"], ["PERSISTENCE_UNAVAILABLE", true, "uncertain"]] as const) {
    const submission = instance.sendInput({ entityId: "a", rule: "example.move", intent: { dx: 9 } });
    instance._dispatch(create(EnvelopeSchema, { payload: { case: "error", value: {
      code, retryable, requestMessageId: submission.request.commandId, message: "denied",
    } } }));
    assert.equal((await submission.result).status, status);
    assert.equal(instance.state.entities.size, 0);
  }
  instance._stopSync(new SyncError("CLOSED"), "closed");
});
it("disconnect settles uncertain input and unchanged request can be retried", async () => {
  const { instance } = fixture();
  const submission = instance.sendInput({ entityId: "a", rule: "example.move", intent: { dx: 1 } });
  instance._stopSync(new SyncError("DISCONNECTED"));
  assert.equal((await submission.result).status, "uncertain");
  assert.equal((instance as any).pendingInputs.size, 0);
});
it("receipt timeout is uncertain and invalid local sends do not leak waiters", async () => {
  const { instance } = fixture();
  assert.throws(() => instance.sendInput({ entityId: "a", rule: "move", intent: {}, commandId: "bad" }));
  assert.equal((instance as any).pendingInputs.size, 0);
  const submission = instance.sendInput({ entityId: "a", rule: "move", intent: {}, timeoutMs: 1 });
  await new Promise(resolve => setTimeout(resolve, 10));
  assert.equal((await submission.result).status, "uncertain");
  assert.equal((instance as any).pendingInputs.size, 0);
  instance._stopSync(new SyncError("CLOSED"), "closed");
});
