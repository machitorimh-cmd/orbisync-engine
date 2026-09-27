import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema, ServerHelloSchema, type Envelope } from "./generated/orbisync/v1/realtime_pb.js";
import { OrbiSyncClient, OrbiSyncConnection, decodeEnvelope, encodeEnvelope } from "./client.js";
import { SyncError, STATE_SYNC_FEATURE } from "./sync_state.js";
class Socket {
  readyState = 1;
  private serverSequence = 2n;
  listeners = new Map<string, Set<(event: Event) => void>>();
  snapshotOnJoin = true;
  addEventListener(name: string, callback: (event: Event) => void) {
    if (!this.listeners.has(name)) this.listeners.set(name, new Set());
    this.listeners.get(name)!.add(callback);
  }
  removeEventListener(name: string, callback: (event: Event) => void) { this.listeners.get(name)?.delete(callback); }
  receive(payload: Envelope["payload"], sequence = this.serverSequence++) {
    const data = encodeEnvelope(create(EnvelopeSchema, { instanceId: "room", sequence, payload }));
    for (const callback of [...this.listeners.get("message") ?? []]) callback({ data } as unknown as Event);
  }
  snapshot() {
    this.receive({ case: "snapshot", value: create(EnvelopeSchema, { payload: { case: "snapshot", value: {
      snapshotId: "s", chunkCount: 1, instanceRevision: 100n,
      data: new TextEncoder().encode('{"format":"orbisync.snapshot.v1","revision":100,"instance":{"instance_id":"room"},"entities":[],"users":[]}'),
    } } }).payload.value as never });
  }
  send(data: Uint8Array) {
    if (decodeEnvelope(data).payload.case === "joinInstance") {
      this.receive(create(EnvelopeSchema, { payload: { case: "joinAccepted", value: { instanceRevision: 99n, resumeToken: "token" } } }).payload);
      if (this.snapshotOnJoin) this.snapshot();
    }
  }
  close() { this.readyState = 3; }
}
function connection(socket: Socket, modern = true) {
  const result = new OrbiSyncConnection(socket as unknown as WebSocket,
    create(ServerHelloSchema, { heartbeatIntervalMs: 20000n, enabledFeatures: modern ? [STATE_SYNC_FEATURE] : [] }),
    new OrbiSyncClient({ baseUrl: "http://localhost" }));
  return result;
}
it("installs sync routing before send, including synchronous JoinAccepted and Snapshot", async () => {
  const socket = new Socket(), conn = connection(socket);
  try {
    const instance = await conn.join("room");
    await instance.ready();
    assert.equal(instance.state.revision, 100n);
    assert.equal(instance.syncStatus, "ready");
    let errors = 0;
    instance.on("error", () => { errors++; });
    instance.on("entitySpawned", () => { throw new Error("application failure"); });
    socket.receive(create(EnvelopeSchema, { payload: { case: "entityCommand", value: {
      operation: "spawn", entityId: "a", instanceRevision: 101n, expectedRevision: 1n, arguments: { kind: "note" },
    } } }).payload);
    assert.equal(instance.state.entities.get("a")?.revision, 1n);
    assert.equal(instance.syncStatus, "ready");
    assert.equal(errors, 1);
  } finally { await conn.disconnect(); }
});
it("entityRevision reads unchanged acknowledgements without taking a state snapshot", async () => {
  const socket = new Socket();
  const conn = connection(socket);
  try {
    const instance = await conn.join("room");
    await instance.ready();
    Object.defineProperty(instance, "state", { get() { throw new Error("full state read"); } });
    assert.equal(instance.entityRevision("a"), undefined);
    for (const revision of [101n, 102n]) {
      socket.receive(create(EnvelopeSchema, { payload: { case: "stateDelta", value: {
        fromRevision: revision - 1n, toRevision: revision,
        entities: [{ entityId: "a", revision, transform: { positionX: 1, rotationW: 1 } }],
      } } }).payload);
      assert.equal(instance.entityRevision("a"), revision);
    }
  } finally { await conn.disconnect(); }
});

it("join acceptance does not resolve ready; leave rejects pending ready", async () => {
  const socket = new Socket(); socket.snapshotOnJoin = false;
  const conn = connection(socket);
  try {
    const instance = await conn.join("room");
    assert.equal(instance.syncStatus, "syncing");
    const pending = assert.rejects(instance.ready(), { code: "CLOSED" });
    await instance.leave();
    await pending;
  } finally { await conn.disconnect(); }
});
it("a quiet old server is explicitly unsupported for strict ready", async () => {
  const socket = new Socket(); socket.snapshotOnJoin = false;
  const conn = connection(socket, false);
  try {
    const instance = await conn.join("room");
    await assert.rejects(instance.ready(), { code: "UNSUPPORTED_SERVER" });
  } finally { await conn.disconnect(); }
});

it("exhaustion settles waiters, releases resources and fences captured socket callbacks", async () => {
  const socket = new Socket(), conn = connection(socket);
  try {
    const instance = await conn.join("room");
    const oldMessages = [...socket.listeners.get("message") ?? []];
    const oldCloses = [...socket.listeners.get("close") ?? []];
    instance._stopSync(new SyncError("DISCONNECTED"));
    const controller = new AbortController();
    const waiting = assert.rejects(instance.ready({ signal: controller.signal }), { code: "RECOVERY_EXHAUSTED" });
    let failures = 0;
    instance.on("syncStateChanged", status => { if (status === "failed") failures++; });
    (conn as any).reconnectAttempt = 5;
    (conn as any).scheduleReconnect();
    await waiting;
    assert.equal(failures, 1);
    assert.equal(instance.syncStatus, "failed");
    for (const name of ["reconnectTimer", "heartbeatTimer", "heartbeatAckTimeout", "reconnectAbortController", "joinWaiterAbortController", "socketHandlers"]) assert.equal((conn as any)[name], null, name);
    assert.equal([...socket.listeners.values()].reduce((n, listeners) => n + listeners.size, 0), 0);
    assert.equal((instance as any).handlers.size, 0);
    assert.equal((instance as any).reliableQueue.length, 0);
    assert.equal((instance as any).latestWinsQueue.size, 0);
    assert.equal((instance as any).sync.waiters.size, 0);
    assert.equal((instance as any).sync.lastSnapshot, null);
    const data = encodeEnvelope(create(EnvelopeSchema, { instanceId: "room", payload: { case: "resumeAccepted", value: { currentRevision: 100n, replayFollows: false } } }));
    oldMessages.forEach(callback => callback({ data } as unknown as Event));
    oldCloses.forEach(callback => callback({ code: 1011 } as unknown as Event));
    conn._requestSyncRecovery(new SyncError("INVALID_UPDATE"));
    controller.abort();
    assert.equal(instance.syncStatus, "failed");
    assert.equal((conn as any).reconnectTimer, null);
    await assert.rejects(instance.ready(), { code: "RECOVERY_EXHAUSTED" });
  } finally { await conn.disconnect(); }
});

it("five failed recovery attempts terminate; explicit leave cannot restart recovery", async () => {
  const socket = new Socket(), conn = connection(socket);
  try {
    const instance = await conn.join("room");
    let attempts = 0;
    (conn as any).computeBackoffMs = () => 0;
    (conn as any).client.ensureValidAccessToken = async () => {};
    (conn as any).client.fetchRealtimeTicket = async () => { attempts++; throw new Error("offline"); };
    const failed = new Promise<void>((resolve, reject) => {
      const deadline = setTimeout(() => reject(new Error("recovery never terminated")), 1000);
      instance.on("syncStateChanged", status => { if (status === "failed") { clearTimeout(deadline); resolve(); } });
    });
    socket.listeners.get("close")!.forEach(callback => callback({ code: 1011 } as unknown as Event));
    await failed;
    assert.equal(attempts, 5);
    await assert.rejects(instance.ready(), { code: "RECOVERY_EXHAUSTED" });
    await instance.leave();
    (conn as any).scheduleReconnect();
    assert.equal(instance.syncStatus, "closed");
    assert.equal((conn as any).reconnectTimer, null);
  } finally { await conn.disconnect(); }
});

it("leave during a pending recovery ticket prevents a late candidate and heartbeat", async () => {
  const socket = new Socket(), conn = connection(socket);
  try {
    const instance = await conn.join("room");
    let release!: (value: string) => void;
    let started!: () => void;
    const ticketStarted = new Promise<void>(resolve => { started = resolve; });
    (conn as any).client.ensureValidAccessToken = async () => {};
    (conn as any).client.fetchRealtimeTicket = () => { started(); return new Promise<string>(resolve => { release = resolve; }); };
    const reconnect = (conn as any).doReconnect();
    await ticketStarted;
    await instance.leave();
    release("late-ticket");
    await reconnect;
    assert.equal(instance.syncStatus, "closed");
    assert.equal((conn as any).heartbeatTimer, null);
    assert.equal((conn as any).reconnectTimer, null);
    assert.equal([...socket.listeners.values()].reduce((n, listeners) => n + listeners.size, 0), 0);
  } finally { await conn.disconnect(); }
});

it("strict recovery still reaches ready via Resume and fresh Join", async () => {
  const OriginalWebSocket = globalThis.WebSocket;
  for (const fresh of [false, true]) {
    class RecoverySocket extends Socket {
      static OPEN = 1; static CONNECTING = 0; static CLOSED = 3;
      constructor() {
        super();
        queueMicrotask(() => this.listeners.get("open")?.forEach(callback => callback(new Event("open"))));
      }
      override send(data: Uint8Array) {
        const payload = decodeEnvelope(data).payload;
        if (payload.case === "clientHello") {
          this.receive(create(EnvelopeSchema, { payload: { case: "serverHello", value: {
            connectionId: "recovered", heartbeatIntervalMs: 20000n, enabledFeatures: [STATE_SYNC_FEATURE],
          } } }).payload, 1n);
        } else if (payload.case === "resumeSession") {
          this.receive(create(EnvelopeSchema, { payload: { case: "resumeAccepted", value: { currentRevision: 100n, replayFollows: true } } }).payload);
          this.snapshot();
        } else super.send(data);
      }
    }
    globalThis.WebSocket = RecoverySocket as unknown as typeof WebSocket;
    const conn = connection(new Socket());
    try {
      const instance = await conn.join("room");
      instance._stopSync(new SyncError("DISCONNECTED"));
      (conn as any).forceFreshJoin = fresh;
      (conn as any).client.ensureValidAccessToken = async () => {};
      (conn as any).client.fetchRealtimeTicket = async () => "ticket";
      await (conn as any).doReconnect();
      assert.equal(instance.syncStatus, "ready", fresh ? "fresh" : "resume");
      assert.equal(instance.state.revision, 100n);
      assert.equal((conn as any).reconnectAttempt, 0);
      assert.notEqual((conn as any).heartbeatTimer, null);
    } finally { await conn.disconnect(); globalThis.WebSocket = OriginalWebSocket; }
  }
});

it("implicit sends use canonical revisions at flush and explicit revisions stay unchanged", async () => {
  const socket = Object.assign(new Socket(), { bufferedAmount: 0 });
  const conn = connection(socket);
  try {
    const instance = await conn.join("room");
    await instance.ready();
    Object.defineProperty(instance, "state", { get() { throw new Error("full state read"); } });
    const sent: Envelope[] = [];
    socket.send = data => { sent.push(decodeEnvelope(data)); };
    const acknowledge = (revision: bigint) => socket.receive(create(EnvelopeSchema, {
      payload: { case: "stateDelta", value: { fromRevision: 100n + revision - 1n,
        toRevision: 100n + revision, entities: [{ entityId: "a", revision,
          transform: { positionX: 1, rotationW: 1 } }] } },
    }).payload);
    acknowledge(1n);
    instance.sendTransform({ entityId: "a", position: { x: 2, y: 0, z: 0 } });
    socket.bufferedAmount = 1_000_000;
    instance.sendEntityCommand({ entityId: "a", operation: "update", args: { value: 1 } });
    instance.sendTransform({ entityId: "a", position: { x: 3, y: 0, z: 0 }, expectedRevision: 7n });
    acknowledge(2n);
    assert.equal(sent.length, 1, "congestion must keep both commands queued");
    socket.bufferedAmount = 0;
    instance._flush();
    const revisions = sent.map(envelope => {
      assert.ok(envelope.payload.case === "entityCommand" || envelope.payload.case === "transformInput");
      return envelope.payload.value.expectedRevision;
    });
    assert.deepEqual(revisions, [1n, 2n, 7n]);
  } finally { await conn.disconnect(); }
});
