import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { OrbiSyncInstance, decodeEnvelope, SyncError, ConfirmedEntityCommands, CONFIRMED_COMMAND_MAX_PENDING } from "./client.js";

function fixture() {
  const sent: Uint8Array[] = [];
  const ws = { bufferedAmount: 0, readyState: 1, send: (bytes: Uint8Array) => sent.push(bytes), close() {} };
  const instance = new OrbiSyncInstance(ws as unknown as WebSocket, "room", () => 1);
  const sync = (generation = 1, revision = 100) => {
    instance._beginSync(generation, true);
    instance._dispatch(create(EnvelopeSchema, { instanceId: "room", payload: { case: "snapshot", value: {
      snapshotId: `s${generation}`, chunkCount: 1, instanceRevision: BigInt(revision),
      data: new TextEncoder().encode(`{"format":"orbisync.snapshot.v1","revision":${revision},"instance":{"instance_id":"room"},"entities":[],"users":[]}`),
    } } }));
  };
  sync();
  const receipt = (id: string, operation = "update", boundary = 101n, entityId = "a") => instance._dispatch(create(EnvelopeSchema, {
    instanceId: "room", payload: { case: "entityCommand", value: { commandId: id, entityId, operation,
      expectedRevision: 2n, instanceRevision: boundary, arguments: operation === "transfer_ownership" ? { new_owner_id: "new-owner" } : { component_key: "example.note", text: "confirmed" } } },
  }));
  const error = (id: string, code = "CONFLICT", retryable = false) => instance._dispatch(create(EnvelopeSchema, { payload: { case: "error", value: { requestMessageId: id, code, retryable } } }));
  return { instance, sent, ws, sync, receipt, error, close: () => instance._stopSync(new SyncError("CLOSED"), "closed") };
}

it("generic confirmations resolve only after canonical state and ignore another entity's receipt", async () => {
  const f = fixture();
  try {
    const command = f.instance.commands.submit({ entityId: "a", operation: "update", args: { text: "request" } });
    f.receipt(command.request.commandId, "update", 101n, "other");
    assert.equal(command.state.status, "pending");
    let observed: bigint | undefined;
    f.instance.commands.onChange(state => { if (state.status === "committed") observed = f.instance.entityRevision("a"); });
    f.receipt(command.request.commandId, "update", 102n);
    assert.equal((await command.result).status, "committed");
    assert.equal(observed, 2n);
    assert.equal(f.instance.commands.metrics.pending, 0);
    assert.equal(f.instance.commands.metrics.bytes, 0);
    assert.throws(() => command.retry());
  } finally { f.close(); }
});

it("retains an immutable exact request through timeout, newer snapshot and explicit same-ID retry", async () => {
  const f = fixture();
  try {
    const args = { nested: { text: "original" } };
    const command = f.instance.commands.submit({ entityId: "a", operation: "update", expectedRevision: 7n, args }, { timeoutMs: 1 });
    args.nested.text = "mutated";
    assert.throws(() => { (command.request.args.nested as { text: string }).text = "changed"; });
    assert.throws(() => { (command as unknown as { request: unknown }).request = {}; });
    await new Promise(resolve => setTimeout(resolve, 10));
    assert.equal((await command.result).status, "unknown");
    assert.equal(f.instance.commands.get(command.request.commandId), command);
    assert.equal(f.instance._getQueueLengths().reliable, 0);
    f.sync(2, 200);
    const retried = command.retry();
    const wires = f.sent.map(decodeEnvelope);
    assert.equal(wires.length, 2);
    assert.equal(wires[0].messageId, wires[1].messageId);
    assert.deepEqual(wires[0].payload, wires[1].payload);
    f.error(command.request.commandId);
    assert.equal((await retried).status, "unknown", "a post-timeout rejection cannot disprove original execution");
    f.receipt(command.request.commandId, "update", 201n);
    assert.equal(command.state.status, "committed");
    assert.equal(f.instance.commands.metrics.pending, 0);
  } finally { f.close(); }
});

it("disconnect preserves unknown tracking but removes automatic transport replays", async () => {
  const f = fixture();
  try {
    f.ws.bufferedAmount = 1_000_000;
    const command = f.instance.commands.submit({ entityId: "a", operation: "delete", expectedRevision: 9n });
    assert.equal(f.instance._getQueueLengths().reliable, 1);
    f.instance._stopSync(new SyncError("DISCONNECTED"));
    assert.equal((await command.result).status, "unknown");
    assert.equal(f.instance._getQueueLengths().reliable, 0);
    assert.throws(() => command.retry(), SyncError);
    f.sync(2, 200); f.ws.bufferedAmount = 0; f.instance._flush();
    assert.equal(f.sent.length, 0);
    const retry = command.retry();
    assert.equal(f.sent.length, 1);
    command.end();
    assert.equal((await retry).status, "ended");
    assert.equal(command.state.status, "ended");
    assert.deepEqual(f.instance.commands.metrics, { pending: 0, bytes: 0 });
  } finally { f.close(); }
});

it("initial rejection is definitive, while a late durable receipt settles an unknown operation", async () => {
  const f = fixture();
  try {
    const denied = f.instance.commands.submit({ entityId: "a", operation: "delete" });
    f.error(denied.request.commandId);
    assert.equal((await denied.result).status, "rejected");
    const unknown = f.instance.commands.submit({ entityId: "a", operation: "update" });
    f.error(unknown.request.commandId, "PERSISTENCE_UNAVAILABLE", true);
    assert.equal((await unknown.result).status, "unknown");
    f.receipt(unknown.request.commandId);
    assert.equal(unknown.state.status, "committed");
  } finally { f.close(); }
});

it("ownership transfer is a canonical receipt rather than a synchronization failure", async () => {
  const f = fixture();
  try {
    const command = f.instance.commands.submit({ entityId: "a", operation: "transfer_ownership", args: { new_owner_id: "new-owner" } });
    f.receipt(command.request.commandId, "transfer_ownership");
    assert.equal((await command.result).status, "committed");
    assert.equal(f.instance.state.entities.get("a")?.ownerId, "new-owner");
    assert.equal(f.instance.syncStatus, "ready");
  } finally { f.close(); }
});

it("close ends pending tracking and instances cannot share a tracked handle", async () => {
  const a = fixture(), b = fixture();
  try {
    const command = a.instance.commands.submit({ entityId: "a", operation: "delete" });
    assert.equal(b.instance.commands.get(command.request.commandId), undefined);
    b.receipt(command.request.commandId);
    assert.equal(command.state.status, "pending");
    a.close();
    assert.equal((await command.result).status, "ended");
    assert.throws(() => command.retry(), SyncError);
    assert.deepEqual(a.instance.commands.metrics, { pending: 0, bytes: 0 });
  } finally { a.close(); b.close(); }
});

it("unknown count and bytes remain bounded, then explicit end releases capacity", () => {
  const transport = { syncStatus: "ready", entityRevision: () => 1n, sendEntityCommand: () => "", _forgetQueuedCommand: () => {} };
  const commands = new ConfirmedEntityCommands(transport);
  for (let i = 0; i < CONFIRMED_COMMAND_MAX_PENDING; i++) commands.submit({ entityId: `e${i}`, operation: "delete" });
  commands._stop(false, "DISCONNECTED");
  assert.equal(commands.metrics.pending, 256);
  assert.throws(() => commands.submit({ entityId: "overflow", operation: "delete" }), SyncError);
  for (const state of commands.pending) commands.get(state.commandId)!.end();
  assert.deepEqual(commands.metrics, { pending: 0, bytes: 0 });
  const handles = [];
  for (let i = 0; i < CONFIRMED_COMMAND_MAX_PENDING; i++) {
    try { handles.push(commands.submit({ entityId: `large${i}`, operation: "update", args: { text: "x".repeat(20_000) } })); }
    catch { break; }
  }
  assert.ok(handles.length > 1 && handles.length < CONFIRMED_COMMAND_MAX_PENDING);
  assert.throws(() => commands.submit({ entityId: "byte-overflow", operation: "update", args: { text: "x".repeat(20_000) } }), SyncError);
  assert.ok(commands.metrics.bytes <= 4 * 1024 * 1024);
  commands._stop(true, "CLOSED");
  assert.deepEqual(commands.metrics, { pending: 0, bytes: 0 });
});

it("non-JSON aliases and invalid timeouts never create retained entries", () => {
  const f = fixture();
  try {
    assert.throws(() => f.instance.commands.submit({ entityId: "a", operation: "update", args: { bytes: new Uint8Array([1]) } }), SyncError);
    assert.throws(() => f.instance.commands.submit({ entityId: "a", operation: "update" }, { timeoutMs: 0 }), SyncError);
    assert.deepEqual(f.instance.commands.metrics, { pending: 0, bytes: 0 });
  } finally { f.close(); }
});
