import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { OrbiSyncInstance, OrbiSyncClient, OrbiSyncConnection, encodeEnvelope, decodeEnvelope, ACTIVITY_FEATURE, DIRECTED_EVENTS_FEATURE, STATE_SYNC_FEATURE, type ReceivedDirectedEvent } from "./client.js";
import { EnvelopeSchema, ServerHelloSchema, type Envelope } from "./generated/orbisync/v1/realtime_pb.js";
import { SyncError } from "./sync_state.js";
const a = "0192d43d-a18a-7fed-8123-0123456789ab", b = "0192d43d-a18a-7fed-8123-1123456789ab";
const features = [STATE_SYNC_FEATURE, ACTIVITY_FEATURE, DIRECTED_EVENTS_FEATURE];
function wire(payload: Envelope["payload"], messageId = ""): Envelope { return create(EnvelopeSchema, { instanceId: "room", messageId, payload }); }
function canonicalSnapshot(): Envelope {
  return wire({ case: "snapshot", value: create(EnvelopeSchema, { payload: { case: "snapshot", value: {
    snapshotId: "s", chunkCount: 1, instanceRevision: 100n,
    data: new TextEncoder().encode('{"format":"orbisync.snapshot.v1","revision":100,"instance":{"instance_id":"room"},"entities":[],"users":[]}'),
  } } }).payload.value as never });
}
function setup(advertised: string[] = features) {
  const sent: Envelope[] = [];
  const socket = { readyState: 1, bufferedAmount: 0, send(data: Uint8Array) { sent.push(decodeEnvelope(data)); }, close() { this.readyState = 3; } };
  let sequence = 0;
  const instance = new OrbiSyncInstance(socket as unknown as WebSocket, "room", () => ++sequence);
  instance._setServerFeatures(advertised); instance._beginSync(1, true); instance._dispatch(canonicalSnapshot());
  return { instance, sent, socket };
}
it("directed events never fall back to broadcast and reject invalid recipients before queue admission", () => {
  const f = setup([STATE_SYNC_FEATURE]);
  assert.throws(() => f.instance.sendDirectedEvent({ eventType: "custom.chat", recipientUserIds: [a] }), { code: "UNSUPPORTED_SERVER" });
  assert.throws(() => f.instance.activities.set({ key: "app.typing" }), { code: "UNSUPPORTED_SERVER" });
  assert.throws(() => f.instance.activities.clear("app.typing"), { code: "UNSUPPORTED_SERVER" });
  assert.equal(f.sent.length, 0);
  f.instance._setServerFeatures(features);
  for (const options of [
    {}, { recipientUserIds: [] }, { recipientUserIds: [a], recipientPresenceIds: [b] },
    { recipientUserIds: [a, a] }, { recipientUserIds: [a.toUpperCase()] }, { recipientPresenceIds: ["not-an-id"] },
  ]) assert.throws(() => f.instance.sendDirectedEvent({ eventType: "custom.chat", ...options }), { code: "INVALID_UPDATE" });
  assert.throws(() => f.instance.sendDirectedEvent({ eventType: "custom.chat", recipientUserIds: Array.from({ length: 65 }, () => a) }), { code: "RESOURCE_LIMIT" });
  for (const eventType of ["core.chat", "custom.", "custom..chat", "custom.chat💡", "custom." + "x".repeat(122)]) assert.throws(() => f.instance.sendDirectedEvent({ eventType, recipientUserIds: [a] }), { code: "INVALID_UPDATE" });
  assert.equal(f.sent.length, 0); f.instance._close();
});
it("directed reliable payload bytes are bounded across in-flight and queued messages", () => {
  const f = setup(); let admitted = 0;
  while (admitted < 100) {
    try { f.instance.sendDirectedEvent({ eventType: "custom.chat", recipientUserIds: [a], data: { text: "x".repeat(60_000) } }); admitted++; }
    catch (error) { assert.equal((error as SyncError).code, "RESOURCE_LIMIT"); break; }
  }
  assert.ok(admitted > 1 && admitted < 100); assert.equal(f.instance.getQueueMetrics().reliable.inFlight, admitted);
  assert.equal(f.sent.filter(item => item.payload.case === "directedEvent").length, admitted); f.instance._close();
});
it("directed recipients are normalized and queued payloads owned; echoes are typed, immutable and deduplicated", () => {
  const f = setup(); f.socket.bufferedAmount = 300_000;
  const data = { message: { text: "private" } }, recipients = [b, a];
  const id = f.instance.sendDirectedEvent({ eventType: "custom.chat", data, recipientUserIds: recipients });
  data.message.text = "changed"; recipients.length = 0;
  f.socket.bufferedAmount = 0; f.instance._flush();
  const sent = f.sent.find(item => item.payload.case === "directedEvent")!;
  assert.equal(sent.messageId, id); assert.equal(sent.payload.case, "directedEvent");
  if (sent.payload.case !== "directedEvent") throw new Error("missing directed event");
  assert.deepEqual(sent.payload.value.recipientUserIds, [a, b]); assert.deepEqual(sent.payload.value.event?.data, { message: { text: "private" } });
  assert.equal(f.instance.getQueueMetrics().reliable.inFlight, 1);
  let received: ReceivedDirectedEvent | undefined, count = 0;
  const unsubscribe = f.instance.onDirectedEvent(value => { received = value; count++; });
  const echoed = wire({ case: "directedEvent", value: { ...sent.payload.value, resolvedPresenceIds: [a] } }, id);
  f.instance._dispatch(echoed); f.instance._dispatch(echoed);
  assert.equal(count, 1); assert.equal(received?.event.eventId, id); assert.ok(Object.isFrozen(received?.event.data)); assert.ok(Object.isFrozen(received?.resolvedPresenceIds));
  assert.equal(f.instance.getQueueMetrics().reliable.inFlight, 0); unsubscribe(); f.instance._close();
});
it("private and activity frames cannot mutate a different joined instance", () => {
  const f = setup(); let errors = 0, directed = 0;
  f.instance.on("error", () => { errors++; }); f.instance.onDirectedEvent(() => { directed++; });
  const other = wire(create(EnvelopeSchema, { payload: { case: "directedEvent", value: { event: { eventId: a, eventType: "custom.chat" }, recipientPresenceIds: [a] } } }).payload, a);
  other.instanceId = "other-room"; f.instance._dispatch(other);
  const changed = wire(create(EnvelopeSchema, { payload: { case: "activityChanged", value: { version: 1n, activity: { presenceId: a, userId: b, key: "app.typing", expiresAtUnixMs: BigInt(Date.now() + 5000) } } } }).payload);
  changed.instanceId = "other-room"; f.instance._dispatch(changed);
  assert.equal(errors, 2); assert.equal(directed, 0); assert.equal(f.instance.activities.current.length, 0); f.instance._close();
});
it("decoded directed data accepts valid wire payloads whose escaped JSON exceeds 64 KiB", () => {
  const f = setup();
  const data = { text: "\u0000".repeat(20_000), sender_user_id: a, sender_presence_id: b };
  assert.ok(new TextEncoder().encode(JSON.stringify(data)).length > 65_536);
  const outgoing = wire(create(EnvelopeSchema, { payload: { case: "directedEvent", value: {
    event: { eventId: a, eventType: "custom.message", instanceRevision: 101n, data },
    recipientUserIds: [b], resolvedPresenceIds: [a],
  } } }).payload, a);
  const bytes = encodeEnvelope(outgoing);
  assert.ok(bytes.length <= 65_536, "the complete wire envelope fits within the Struct limit");
  let received: ReceivedDirectedEvent | undefined;
  f.instance.onDirectedEvent(event => { received = event; });
  f.instance._dispatch(decodeEnvelope(bytes));
  assert.deepEqual(received?.event.data, data);
  assert.ok(Object.isFrozen(received?.event.data));
  f.instance._close();
});
it("directed events replay the same ID only for resumed sessions and discard uncertain or unsupported deliveries", () => {
  const f = setup(), errors: { code: string; requestMessageId: string }[] = [];
  f.instance.on("error", error => errors.push(error as never));
  const id = f.instance.sendDirectedEvent({ eventType: "custom.chat", recipientPresenceIds: [a] });
  f.instance._stopSync(new SyncError("DISCONNECTED"));
  f.instance._updateWebSocket(f.socket as unknown as WebSocket); f.instance._beginSync(2, true); f.instance._dispatch(canonicalSnapshot()); f.instance._flush();
  assert.equal(f.sent.filter(item => item.payload.case === "directedEvent" && item.messageId === id).length, 2);
  f.instance._freshSession(); f.instance._flush();
  assert.deepEqual(errors.map(item => [item.code, item.requestMessageId]), [["DELIVERY_UNKNOWN", id]]);
  assert.equal(f.instance.getQueueMetrics().reliable.inFlight, 0);
  f.socket.bufferedAmount = 300_000;
  const queued = f.instance.sendDirectedEvent({ eventType: "custom.chat", recipientPresenceIds: [a] });
  f.instance._setServerFeatures([STATE_SYNC_FEATURE]); f.socket.bufferedAmount = 0; f.instance._flush();
  assert.equal(f.sent.some(item => item.messageId === queued), false); assert.equal(errors.at(-1)?.code, "UNSUPPORTED_SERVER"); f.instance._close();
});
it("activity transport requests a fresh snapshot after ready, never replays updates, and rejects offline/congested sends", () => {
  const f = setup();
  assert.equal(f.sent.filter(item => item.payload.case === "activitySnapshotRequest").length, 1);
  f.instance.activities.set({ key: "app.typing", data: { entity: a } }); f.instance.activities.clear("app.typing");
  assert.equal(f.instance.getQueueMetrics().reliable.inFlight, 0);
  assert.deepEqual(f.sent.filter(item => item.payload.case === "activityUpdate").map(item => item.payload.case === "activityUpdate" && item.payload.value.ttlMs), [5000, 0]);
  f.socket.bufferedAmount = 300_000;
  assert.throws(() => f.instance.activities.set({ key: "app.typing" }), { code: "RESOURCE_LIMIT" });
  f.socket.bufferedAmount = 0;
  f.instance._dispatch(wire({ case: "activitySnapshot", value: { activities: [{ presenceId: a, userId: b, key: "app.typing", data: {}, expiresAtUnixMs: BigInt(Date.now() + 5000) }], version: 1n, snapshotId: f.sent.find(item => item.payload.case === "activitySnapshotRequest")!.messageId, chunkCount: 1, chunkIndex: 0 } } as never));
  assert.equal(f.instance.activities.current.length, 1);
  f.instance._stopSync(new SyncError("DISCONNECTED")); assert.equal(f.instance.activities.current.length, 0);
  assert.throws(() => f.instance.activities.set({ key: "app.typing" }), { code: "NOT_READY" });
  f.instance._updateWebSocket(f.socket as unknown as WebSocket); f.instance._beginSync(2, true); f.instance._dispatch(canonicalSnapshot()); f.instance._flush();
  assert.equal(f.sent.filter(item => item.payload.case === "activitySnapshotRequest").length, 2);
  assert.equal(f.sent.filter(item => item.payload.case === "activityUpdate").length, 2); f.instance._close();
});
it("fresh membership discards an unsent private request once while retaining an unsent public room event", () => {
  const f = setup(), errors: { code: string; requestMessageId: string; retryable: boolean }[] = [];
  f.sent.length = 0; f.socket.bufferedAmount = 300_000;
  f.instance.on("error", error => errors.push(error as never));
  const privateId = f.instance.sendDirectedEvent({ eventType: "custom.message", recipientUserIds: [a], data: { text: "old membership" } });
  const publicId = f.instance.sendDomainEvent({ eventType: "custom.notice", data: { text: "room" } });
  assert.equal(f.sent.length, 0); assert.equal(f.instance.getQueueMetrics().reliable.inFlight, 0);
  f.instance._freshSession(); f.instance._freshSession();
  f.socket.bufferedAmount = 0; f.instance._flush();
  assert.equal(f.sent.filter(item => item.payload.case === "directedEvent").length, 0);
  assert.equal(f.sent.some(item => item.messageId === privateId), false);
  assert.equal(f.sent.filter(item => item.messageId === publicId && item.payload.case === "domainEvent").length, 1);
  assert.deepEqual(errors.map(error => [error.code, error.requestMessageId, error.retryable]), [["SESSION_CHANGED", privateId, false]]);
  f.instance._close();
});
it("real connection initial-delivery ordering requests activity only after canonical ready and Active", async () => {
  class Socket {
    readyState = 1; bufferedAmount = 0; sequence = 2n;
    activityRequested: (() => void) | undefined;
    listeners = new Map<string, Set<(event: Event) => void>>(); sent: Envelope[] = [];
    addEventListener(name: string, callback: (event: Event) => void) { if (!this.listeners.has(name)) this.listeners.set(name, new Set()); this.listeners.get(name)!.add(callback); }
    removeEventListener(name: string, callback: (event: Event) => void) { this.listeners.get(name)?.delete(callback); }
    receive(envelope: Envelope) { envelope.sequence = this.sequence++; const data = encodeEnvelope(envelope); for (const handler of this.listeners.get("message") ?? []) handler({ data } as unknown as Event); }
    send(data: Uint8Array) {
      const envelope = decodeEnvelope(data); this.sent.push(envelope);
      if (envelope.payload.case === "joinInstance") {
        this.receive(wire({ case: "joinAccepted", value: create(EnvelopeSchema, { payload: { case: "joinAccepted", value: { presenceId: a, userId: b, instanceId: "room", instanceRevision: 99n } } }).payload.value as never }));
        this.receive(canonicalSnapshot());
      }
      if (envelope.payload.case === "activitySnapshotRequest") {
        this.receive(wire({ case: "activitySnapshot", value: { activities: [], version: 2n, snapshotId: envelope.messageId, chunkIndex: 0, chunkCount: 1 } } as never));
        this.activityRequested?.();
      }
    }
    close() { this.readyState = 3; }
  }
  const socket = new Socket(), connection = new OrbiSyncConnection(socket as unknown as WebSocket,
    create(ServerHelloSchema, { enabledFeatures: features, heartbeatIntervalMs: 20_000n }), new OrbiSyncClient({ baseUrl: "http://localhost" }));
  try {
    const instance = await connection.join("room");
    assert.equal(socket.sent.some(item => item.payload.case === "activitySnapshotRequest"), false);
    await instance.ready();
    assert.equal(socket.sent.filter(item => item.payload.case === "activitySnapshotRequest").length, 1);
    assert.equal(connection.getConnectionState().sessionState, "Active");
    instance.activities.set({ key: "app.typing" });
    socket.receive(wire(create(EnvelopeSchema, { payload: { case: "directedEvent", value: { event: { eventId: a, eventType: "custom.message", instanceRevision: 120n }, recipientPresenceIds: [a], resolvedPresenceIds: [a] } } }).payload, a));
    assert.equal(connection.getConnectionState().lastAppliedRevision, 120n, "private events must advance the resume cursor even without public events");
    const uncertain: { code: string; requestMessageId: string }[] = [];
    instance.on("error", error => uncertain.push(error as never));
    const oldEvent = instance.sendDirectedEvent({ eventType: "custom.message", recipientPresenceIds: [a] });
    // A same-socket fresh Join must invalidate Activity's old request/view,
    // even if the canonical snapshot is one chunk at the same boundary.
    await new Promise<void>((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("fresh activity snapshot was not requested")), 1000);
      socket.activityRequested = () => { clearTimeout(timer); resolve(); };
      socket.receive(wire(create(EnvelopeSchema, { payload: { case: "resyncRequired", value: {} } }).payload));
    });
    assert.equal(socket.sent.filter(item => item.payload.case === "activitySnapshotRequest").length, 2);
    assert.equal(connection.getConnectionState().sessionState, "Active");
    instance._flush();
    assert.equal(instance.getQueueMetrics().reliable.inFlight, 0);
    assert.deepEqual(uncertain.map(error => [error.code, error.requestMessageId]), [["DELIVERY_UNKNOWN", oldEvent]]);
    assert.equal(socket.sent.filter(item => item.messageId === oldEvent).length, 1, "fresh membership cannot replay a possibly accepted private event");
  } finally { await connection.disconnect(); }
});
