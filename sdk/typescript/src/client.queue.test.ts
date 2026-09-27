import { describe, it, afterEach } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { SyncError } from "./sync_state.js";
import {
  OrbiSyncInstance,
  OrbiSyncConnection,
  decodeEnvelope,
  SEND_QUEUE_BUFFERED_THRESHOLD,
  LATEST_WINS_MAX_KEYS,
  RELIABLE_MAX_QUEUE,
  ReliableQueueOverflowError,
} from "./client.js";

// Minimal fake WebSocket for queue tests. bufferedAmount is controllable.
class FakeWs {
  sent: Uint8Array[] = [];
  bufferedAmount = 0;
  readyState = 1; // OPEN
  binaryType: string = "arraybuffer";
  private listeners = new Map<string, Set<(ev: unknown) => void>>();
  send(data: Uint8Array): void {
    // Clone bytes
    const copy = new Uint8Array(data);
    this.sent.push(copy);
    // Simulate bufferedAmount growth — but we control externally for tests,
    // so don't auto-increase here. Tests set bufferedAmount explicitly.
  }
  close(): void {
    this.readyState = 3;
  }
  addEventListener(event: string, handler: (ev: unknown) => void): void {
    let set = this.listeners.get(event);
    if (!set) {
      set = new Set();
      this.listeners.set(event, set);
    }
    set.add(handler);
  }
  removeEventListener(event: string, handler: (ev: unknown) => void): void {
    this.listeners.get(event)?.delete(handler);
  }
  // helper to dispatch close if needed
}

function makeInstance(ws: FakeWs, instanceId = "inst-1"): OrbiSyncInstance {
  let seq = 0;
  const next = () => {
    seq += 1;
    return seq;
  };
  // FakeWs is compatible with WebSocket via cast
  return new OrbiSyncInstance(ws as unknown as WebSocket, instanceId, next);
}

it("bounds aggregate queued payload bytes and releases them on leave", async () => {
  const ws = new FakeWs();
  ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD;
  const instance = makeInstance(ws);
  let accepted = 0;
  assert.throws(() => {
    for (; accepted < RELIABLE_MAX_QUEUE; accepted++) instance.sendEntityCommand({ entityId: "a", operation: "update", args: { text: "x".repeat(25000) } });
  }, { code: "RESOURCE_LIMIT" });
  assert.ok(accepted > 1 && accepted < RELIABLE_MAX_QUEUE);
  assert.equal(instance._getQueueLengths().reliable, accepted);
  await instance.leave();
  assert.equal(instance._getQueueLengths().reliable, 0);
  assert.equal(instance._hasFlushTimer(), false);
});

it("rejects unsafe Struct integers before protobuf encoding", () => {
  const ws = new FakeWs(), instance = makeInstance(ws);
  for (const value of [9007199254740992, 9007199254740993n, Infinity]) {
    assert.throws(() => instance.sendEntityCommand({ entityId: "a", operation: "update", args: { nested: [value] } }), { code: "INVALID_UPDATE" });
  }
  assert.equal(ws.sent.length, 0);
});

it("leaving discards pending sends and rejects subsequent use of the instance", async () => {
  const ws = new FakeWs();
  ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD;
  const instance = makeInstance(ws);
  instance.sendDomainEvent({ eventType: "custom.chat", data: { text: "pending" } });
  await instance.leave();
  assert.equal(instance.getQueueMetrics().reliable.queueLength, 0);
  assert.throws(() => instance.sendDomainEvent({ eventType: "custom.chat" }), /closed/);
  assert.throws(() => instance.sendEntityCommand({ entityId: "entity", operation: "delete" }), /closed/);
  assert.throws(() => instance.sendTransform({ entityId: "entity", position: { x: 0, y: 0, z: 0 } }), /closed/);
});

it("retains unacknowledged reliable sends across transport replacement", async () => {
  const first = new FakeWs();
  const instance = makeInstance(first);
  const id = instance.sendDomainEvent({ eventType: "custom.chat", data: { text: "in flight" } });
  const sent = decodeEnvelope(first.sent[0]);
  assert.equal(id, sent.messageId);
  assert.equal(instance.getQueueMetrics().reliable.inFlight, 1);
  const replacement = new FakeWs();
  instance._updateWebSocket(replacement as unknown as WebSocket);
  instance._flush();
  assert.equal(replacement.sent.length, 1, "a successful ws.send is not server acknowledgement");
  assert.equal(decodeEnvelope(replacement.sent[0]).messageId, sent.messageId);
  instance._dispatch(sent);
  assert.equal(instance.getQueueMetrics().reliable.inFlight, 0);
  const third = new FakeWs();
  instance._updateWebSocket(third as unknown as WebSocket);
  instance._flush();
  assert.equal(third.sent.length, 0, "acknowledged events must not be sent again");
  await instance.leave();
});

it("fresh sessions report uncertain event delivery and retain unsent events and idempotent commands", async () => {
  const first = new FakeWs();
  const instance = makeInstance(first);
  const errors: Array<{ code: string; requestMessageId: string }> = [];
  instance.on("error", value => errors.push(value as { code: string; requestMessageId: string }));
  instance.sendDomainEvent({ eventType: "custom.chat", data: { text: "uncertain" } });
  instance.sendEntityCommand({ entityId: "entity", operation: "delete" });
  first.readyState = 3;
  instance.sendDomainEvent({ eventType: "custom.chat", data: { text: "not sent yet" } });
  instance._stopSync(new SyncError("DISCONNECTED"));
  assert.equal(instance.getQueueMetrics().reliable.queueLength, 1);
  const replacement = new FakeWs();
  instance._updateWebSocket(replacement as unknown as WebSocket);
  instance._freshSession();
  instance._flush();
  assert.equal(errors.length, 1);
  assert.equal(errors[0].code, "DELIVERY_UNKNOWN");
  assert.equal(errors[0].requestMessageId, decodeEnvelope(first.sent[0]).messageId);
  assert.deepEqual(replacement.sent.map(bytes => decodeEnvelope(bytes).payload.case), ["entityCommand", "domainEvent"]);
  await instance.leave();
});

it("an explicit rejection acknowledges a failed request and frees send capacity", async () => {
  const ws = new FakeWs();
  const instance = makeInstance(ws);
  instance.sendDomainEvent({ eventType: "custom.chat" });
  const sent = decodeEnvelope(ws.sent[0]);
  instance._dispatch(create(EnvelopeSchema, { payload: { case: "error", value: { code: "NOT_JOINED", requestMessageId: sent.messageId } } }));
  const replacement = new FakeWs();
  instance._updateWebSocket(replacement as unknown as WebSocket);
  instance._flush();
  assert.equal(replacement.sent.length, 0);
  await instance.leave();
});

it("handler exceptions notify error listeners without stopping delivery or recursing", async () => {
  const instance = makeInstance(new FakeWs());
  let notifications = 0;
  let delivered = 0;
  instance.on("domainEvent", () => { throw new Error("application failure"); });
  instance.on("domainEvent", () => delivered++);
  instance.on("error", () => { throw new Error("error handler failure"); });
  instance.on("error", value => {
    assert.equal((value as { code: string }).code, "EVENT_HANDLER_FAILED");
    notifications++;
  });
  instance._dispatch(create(EnvelopeSchema, { payload: { case: "domainEvent", value: { eventType: "custom.chat" } } }));
  assert.equal(delivered, 1);
  assert.equal(notifications, 1);
  await instance.leave();
});

describe("snapshot revision wiring", () => {
  it("deduplicates replayed reliable events and emits a complete snapshot separately from raw chunks", () => {
    const instance = makeInstance(new FakeWs());
    const events: unknown[] = [];
    const snapshots: unknown[] = [];
    instance.on("domainEvent", value => events.push(value));
    instance.on("snapshotApplied", value => snapshots.push(value));
    const event = create(EnvelopeSchema, { messageId: "stable-event-id", payload: { case: "domainEvent", value: { eventId: "id", eventType: "custom.chat.message", instanceRevision: 4n } } });
    instance._dispatch(event);
    instance._dispatch(event);
    assert.equal(events.length, 1);
    instance._dispatch(create(EnvelopeSchema, { payload: { case: "snapshot", value: { snapshotId: "state", chunkCount: 1, data: new TextEncoder().encode('{"entities":[]}') } } }));
    assert.deepEqual(snapshots, [{ entities: [] }]);
  });
  it("reports missing chunks and triggers reconnect after the assembly deadline", (t) => {
    t.mock.timers.enable({ apis: ["setTimeout"] });
    const instance = makeInstance(new FakeWs());
    let reconnects = 0;
    instance._setConnection({ _forceCloseTransport: () => { reconnects++; } } as unknown as OrbiSyncConnection);
    const errors: unknown[] = [];
    instance.on("error", error => errors.push(error));
    instance._dispatch(create(EnvelopeSchema, { payload: { case: "snapshot", value: {
      snapshotId: "partial", chunkCount: 2, chunkIndex: 0, data: new TextEncoder().encode('{"entities":['),
    } } }));
    t.mock.timers.tick(10_001);
    assert.equal(reconnects, 1);
    assert.equal((errors[0] as { code: string }).code, "INVALID_SNAPSHOT");
  });
  it("does not roll an entity back when a stale delta follows a newer update", () => {
    const ws = new FakeWs();
    const instance = makeInstance(ws);
    const updates: unknown[] = [];
    instance.on("entityUpdated", value => updates.push(value));
    for (const revision of [8n, 7n, 8n]) instance._dispatch(create(EnvelopeSchema, {
      payload: { case: "stateDelta", value: { toRevision: revision, entities: [{ entityId: "entity", revision }] } },
    }));
    assert.equal(updates.length, 1);
    instance._dispatch(create(EnvelopeSchema, { payload: { case: "entityCommand", value: {
      entityId: "entity", commandId: "old-result", operation: "update", expectedRevision: 4n,
    } } }));
    instance.sendEntityCommand({ entityId: "entity", operation: "update" });
    const payload = decodeEnvelope(ws.sent[0]!).payload;
    assert.equal(payload.case === "entityCommand" && payload.value.expectedRevision, 8n);
  });
  it("applies a multi-chunk snapshot once, refreshes stale revisions and removes deleted entities", () => {
    const ws = new FakeWs();
    const instance = makeInstance(ws);
    instance._setEntityRevision("entity", 1n);
    instance._setEntityRevision("deleted", 5n);
    const bytes = new TextEncoder().encode(JSON.stringify({ entities: [{ entity_id: "entity", revision: 7, label: "こんにちは" }] }));
    const cut = bytes.length - 7; // Deliberately splits a multibyte string.
    let chunks = 0;
    instance.on("snapshot", () => chunks++);
    const snapshot = (index: number, data: Uint8Array) => create(EnvelopeSchema, {
      payload: { case: "snapshot", value: { snapshotId: "test", chunkIndex: index, chunkCount: 2, instanceRevision: 11n, data } },
    });
    instance._dispatch(snapshot(0, bytes.slice(0, cut)));
    instance.sendEntityCommand({ entityId: "entity", operation: "update" });
    instance._dispatch(snapshot(1, bytes.slice(cut)));
    instance.sendEntityCommand({ entityId: "entity", operation: "update" });
    instance.sendEntityCommand({ entityId: "deleted", operation: "update" });
    assert.equal(chunks, 2);
    assert.deepEqual(ws.sent.map(bytes => {
      const payload = decodeEnvelope(bytes).payload;
      assert.equal(payload.case, "entityCommand");
      return payload.case === "entityCommand" ? payload.value.expectedRevision : -1n;
    }), [1n, 7n, 0n]);
  });

  it("reports an invalid snapshot without replacing known entity revisions", () => {
    const ws = new FakeWs();
    const instance = makeInstance(ws);
    instance._setEntityRevision("entity", 4n);
    const errors: unknown[] = [];
    instance.on("error", error => errors.push(error));
    instance._dispatch(create(EnvelopeSchema, { payload: { case: "snapshot", value: {
      snapshotId: "bad", chunkCount: 1, data: new TextEncoder().encode("invalid JSON"),
    } } }));
    instance.sendEntityCommand({ entityId: "entity", operation: "update" });
    assert.equal((errors[0] as { code: string }).code, "INVALID_SNAPSHOT");
    const payload = decodeEnvelope(ws.sent[0]!).payload;
    assert.equal(payload.case === "entityCommand" && payload.value.expectedRevision, 4n);
  });
});

function makeConnectionWithFakeWs(fakeWs: FakeWs): OrbiSyncConnection {
  // Build a minimal connection by constructing with fakeWs and a dummy serverHello/client.
  // Use private constructor via type hack: OrbiSyncConnection expects real WebSocket and ServerHello.
  // Instead we create instance via direct prototype and patch fields, or use OrbiSyncConnection constructor
  // with fakeWs as WebSocket. Need a client stub.
  const fakeClient = {
    getBaseUrl: () => "http://localhost",
    getWsPath: () => "/ws",
    getSubprotocol: () => "orbisync.v1.protobuf",
    getClientName: () => "@orbisync/client",
    getClientVersion: () => "0.1.0",
    getClientType: () => "desktop",
    ensureValidAccessToken: async () => {},
    fetchRealtimeTicket: async () => "ticket",
  } as unknown as import("./client.js").OrbiSyncClient;

  const serverHello = {
    negotiatedMinor: 0,
    connectionId: "conn-1",
    heartbeatIntervalMs: 20000n,
    serverTimeUnixMs: BigInt(Date.now()),
    negotiatedCompression: "",
    enabledFeatures: [],
  } as unknown as import("./client.js").ServerHello;

  // Use constructor directly; it will attach handlers and start heartbeat timer.
  const conn = new OrbiSyncConnection(fakeWs as unknown as WebSocket, serverHello, fakeClient);
  // Stop the periodic heartbeat timer so it doesn't interfere with deterministic tests.
  (conn as unknown as { stopHeartbeat: () => void }).stopHeartbeat();
  // Also clear any heartbeatAckTimeout if set
  return conn;
}

describe("F6 latest-wins queue (SDK-02)", () => {
  // Ensure timers don't leak between tests — clear any interval left on instance.
  afterEach(() => {
    // No global cleanup needed; each test discards its instance.
  });

  it("(a) same (entity_id, component) coalesces to 1 when congested", () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1; // congested
    const inst = makeInstance(ws);
    inst.sendTransform({ entityId: "e1", position: { x: 1, y: 0, z: 0 } });
    inst.sendTransform({ entityId: "e1", position: { x: 2, y: 0, z: 0 } });
    inst.sendTransform({ entityId: "e1", position: { x: 3, y: 0, z: 0 } });

    // While congested, nothing sent yet, queue length 1
    assert.equal(ws.sent.length, 0, "should not send while congested");
    const m1 = inst.getQueueMetrics();
    assert.equal(m1.latestWins.queueLength, 1, "same entity should coalesce to 1");
    assert.equal(m1.latestWins.droppedUpdates, 2, "two older updates should be counted as dropped");
    assert.equal(inst._hasFlushTimer(), true, "flush timer should be active while congested with pending");

    // Flush when uncongested — should send 1 with latest position x=3
    ws.bufferedAmount = 0;
    inst._flush();
    assert.equal(ws.sent.length, 1, "should have flushed 1 coalesced update");
    assert.equal(inst._hasFlushTimer(), false, "flush timer should stop after queue drained");

    const env = decodeEnvelope(ws.sent[0]!);
    assert.equal(env.payload.case, "transformInput");
    if (env.payload.case === "transformInput") {
      assert.equal(env.payload.value.entityId, "e1");
      assert.equal(env.payload.value.transform!.positionX, 3);
    }
    const m2 = inst.getQueueMetrics();
    assert.equal(m2.latestWins.queueLength, 0);
    inst._clearFlushTimer();
  });

  it("(b) different entities are not coalesced", () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 10;
    const inst = makeInstance(ws);
    inst.sendTransform({ entityId: "a", position: { x: 1, y: 0, z: 0 } });
    inst.sendTransform({ entityId: "b", position: { x: 2, y: 0, z: 0 } });
    const m = inst.getQueueMetrics();
    assert.equal(m.latestWins.queueLength, 2, "different entities should be separate keys");
    assert.equal(m.latestWins.droppedUpdates, 0);
    assert.equal(ws.sent.length, 0);
    // Flush both
    ws.bufferedAmount = 0;
    inst._flush();
    assert.equal(ws.sent.length, 2);
    const ids = ws.sent.map((b) => {
      const e = decodeEnvelope(b);
      assert.equal(e.payload.case, "transformInput");
      return (e.payload as { case: "transformInput"; value: { entityId: string } }).value.entityId;
    });
    assert.ok(ids.includes("a") && ids.includes("b"));
    inst._clearFlushTimer();
  });

  it("(c) flushes queued updates when congestion clears", async () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 5;
    const inst = makeInstance(ws);
    inst.sendTransform({ entityId: "e1", position: { x: 10, y: 0, z: 0 } });
    assert.equal(ws.sent.length, 0);
    assert.equal(inst.getQueueMetrics().latestWins.queueLength, 1);
    // Simulate congestion cleared — next flush should send
    ws.bufferedAmount = 0;
    // Timer would flush every 50ms; we trigger manually
    inst._flush();
    assert.equal(ws.sent.length, 1, "queued update should be sent after congestion clears");
    // Also verify that subsequent direct send works without queueing
    inst.sendTransform({ entityId: "e2", position: { x: 20, y: 0, z: 0 } });
    assert.equal(ws.sent.length, 2, "direct send after uncongested should go immediately");
    inst._clearFlushTimer();
  });

  it("(d) reliable queue saturates at 256, throws and preserves 256", () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1; // congested so reliable queues
    const inst = makeInstance(ws);
    for (let i = 0; i < RELIABLE_MAX_QUEUE; i++) {
      inst.sendEntityCommand({ entityId: `e${i}`, operation: "spawn" });
    }
    const m1 = inst.getQueueMetrics();
    assert.equal(m1.reliable.queueLength, 256);
    assert.equal(m1.reliable.saturationErrors, 0);
    assert.equal(ws.sent.length, 0, "reliable queued while congested");

    // 257th should throw ReliableQueueOverflowError and not drop existing
    assert.throws(
      () => inst.sendEntityCommand({ entityId: "overflow", operation: "spawn" }),
      (err: unknown) => {
        assert.ok(err instanceof ReliableQueueOverflowError || (err as Error).message.includes("reliable queue saturated"));
        return true;
      },
    );
    const m2 = inst.getQueueMetrics();
    assert.equal(m2.reliable.queueLength, 256, "existing 256 must not be dropped on overflow");
    assert.equal(m2.reliable.saturationErrors, 1, "saturation error counter should increment");

    // Flush should send 256
    ws.bufferedAmount = 0;
    inst._flush();
    assert.equal(ws.sent.length, 256);
    assert.equal(inst.getQueueMetrics().reliable.queueLength, 0);
    // Verify all are entityCommand
    for (const b of ws.sent) {
      const e = decodeEnvelope(b);
      assert.equal(e.payload.case, "entityCommand");
    }
    inst._clearFlushTimer();
  });

  it("(e) heartbeat is sent even while congested (control priority)", async () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 100; // congested
    // Create a connection so we can test heartbeat path (control queue)
    const conn = makeConnectionWithFakeWs(ws);
    // Queue a latest-wins update to prove latest-wins stays queued
    const inst = makeInstance(ws, "inst-hb");
    // Share the same ws object between inst and conn for realistic congestion
    inst.sendTransform({ entityId: "e1", position: { x: 1, y: 0, z: 0 } });
    assert.equal(ws.sent.length, 0, "latest-wins should be queued while congested");
    assert.equal(inst.getQueueMetrics().latestWins.queueLength, 1);

    // Heartbeat via connection should still send despite congestion.
    // Call private sendHeartbeat via any-cast — no test-only public API on the production class.
    // The helper makeConnectionWithFakeWs is the only test-side helper; production class has no _test* methods.
    (conn as unknown as { sendHeartbeat: () => void }).sendHeartbeat();
    // At least one heartbeat should have been sent even though bufferedAmount high
    const heartbeatSents = ws.sent.filter((b) => {
      try {
        const e = decodeEnvelope(b);
        return e.payload.case === "heartbeat";
      } catch {
        return false;
      }
    });
    assert.ok(heartbeatSents.length >= 1, "heartbeat should be sent even while congested");
    // Latest-wins should still be queued (not sent) because we didn't clear congestion
    const latestRemaining = inst.getQueueMetrics().latestWins.queueLength;
    assert.equal(latestRemaining, 1, "latest-wins should remain queued while heartbeat bypasses");

    // Cleanup
    inst._clearFlushTimer();
    await (conn as unknown as { disconnect: () => Promise<void> }).disconnect();
  });

  it("encodes ownership transfer through the reliable EntityCommand path", () => {
    const ws = new FakeWs();
    const inst = makeInstance(ws, "inst-transfer");
    inst.transferEntityOwnership({
      entityId: "0199f4c2-5f5d-7c8a-9123-0123456789ab",
      newOwnerId: "0199f4c2-5f5d-7c8a-9123-1123456789ab",
      expectedRevision: 7n,
    });

    assert.equal(ws.sent.length, 1);
    const envelope = decodeEnvelope(ws.sent[0]!);
    assert.equal(envelope.payload.case, "entityCommand");
    if (envelope.payload.case === "entityCommand") {
      assert.equal(envelope.payload.value.operation, "transfer_ownership");
      assert.equal(envelope.payload.value.expectedRevision, 7n);
      assert.deepEqual(envelope.payload.value.arguments, {
        new_owner_id: "0199f4c2-5f5d-7c8a-9123-1123456789ab",
      });
    }
    inst._clearFlushTimer();
  });

  it("(f) latest-wins 1024 cap drops oldest and increments counter", () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1;
    const inst = makeInstance(ws);
    for (let i = 0; i < LATEST_WINS_MAX_KEYS; i++) {
      inst.sendTransform({ entityId: `e${i}`, position: { x: i, y: 0, z: 0 } });
    }
    assert.equal(inst.getQueueMetrics().latestWins.queueLength, 1024);
    assert.equal(inst.getQueueMetrics().latestWins.capacityDrops, 0);
    // One more distinct key should evict oldest (e0)
    inst.sendTransform({ entityId: "e_new", position: { x: 999, y: 0, z: 0 } });
    const m = inst.getQueueMetrics();
    assert.equal(m.latestWins.queueLength, 1024, "should stay at cap 1024");
    assert.equal(m.latestWins.capacityDrops, 1, "oldest key drop should increment counter");
    // Flush and verify e0 is gone, e_new is present
    ws.bufferedAmount = 0;
    inst._flush();
    assert.equal(ws.sent.length, 1024);
    const ids = new Set(
      ws.sent.map((b) => {
        const e = decodeEnvelope(b);
        assert.equal(e.payload.case, "transformInput");
        return (e.payload as { case: "transformInput"; value: { entityId: string } }).value.entityId;
      }),
    );
    assert.ok(!ids.has("e0"), "oldest key e0 should have been dropped");
    assert.ok(ids.has("e_new"), "new key should be present");
    assert.ok(ids.has("e1"), "e1 should still be present (not dropped)");
    inst._clearFlushTimer();
  });

  it("flush timer only runs while congested with pending", () => {
    const ws = new FakeWs();
    ws.bufferedAmount = 0; // not congested
    const inst = makeInstance(ws);
    inst.sendTransform({ entityId: "e1", position: { x: 1, y: 0, z: 0 } });
    assert.equal(ws.sent.length, 1, "immediate send when not congested");
    assert.equal(inst._hasFlushTimer(), false, "no timer when not congested");
    // Now congest and queue
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1;
    inst.sendTransform({ entityId: "e2", position: { x: 2, y: 0, z: 0 } });
    assert.equal(inst._hasFlushTimer(), true, "timer should start when congested with pending");
    ws.bufferedAmount = 0;
    inst._flush();
    assert.equal(inst._hasFlushTimer(), false, "timer should stop after drain");
    inst._clearFlushTimer();
  });

  it("assigns contiguous wire sequences after coalescing and control priority", async () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1;
    const connection = makeConnectionWithFakeWs(ws);
    const sequencing = connection as unknown as {
      nextSequence(): number;
      sendHeartbeat(): void;
    };
    const instance = new OrbiSyncInstance(
      ws as unknown as WebSocket, "instance", () => sequencing.nextSequence(),
    );
    try {
      instance.sendTransform({ entityId: "moving", position: { x: 1, y: 0, z: 0 } });
      instance.sendTransform({ entityId: "moving", position: { x: 2, y: 0, z: 0 } });
      instance.sendEntityCommand({ entityId: "created", operation: "spawn" });
      sequencing.sendHeartbeat();
      ws.bufferedAmount = 0;
      instance._flush();

      const frames = ws.sent.map(decodeEnvelope);
      assert.deepEqual(frames.map((frame) => frame.payload.case), [
        "heartbeat", "entityCommand", "transformInput",
      ]);
      assert.deepEqual(frames.map((frame) => frame.sequence), [1n, 2n, 3n]);
    } finally {
      instance._clearFlushTimer();
      await connection.disconnect();
    }
  });

  it("does not consume a sequence for a rejected reliable enqueue", () => {
    const ws = new FakeWs();
    ws.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1;
    const instance = makeInstance(ws);
    try {
      for (let i = 0; i < RELIABLE_MAX_QUEUE; i++) {
        instance.sendDomainEvent({ eventType: "queued", data: { index: i } });
      }
      assert.throws(() => instance.sendDomainEvent({ eventType: "overflow" }), ReliableQueueOverflowError);
      ws.bufferedAmount = 0;
      instance._flush();
      assert.throws(() => instance.sendDomainEvent({ eventType: "still-unacknowledged" }), ReliableQueueOverflowError);
      instance._dispatch(decodeEnvelope(ws.sent[0]));
      instance.sendDomainEvent({ eventType: "after-drain" });
      assert.deepEqual(ws.sent.map((bytes) => decodeEnvelope(bytes).sequence),
        Array.from({ length: RELIABLE_MAX_QUEUE + 1 }, (_, index) => BigInt(index + 1)));
    } finally {
      instance._clearFlushTimer();
    }
  });

  it("numbers pending messages in the replacement connection and owns queued arguments", () => {
    const oldSocket = new FakeWs();
    oldSocket.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD + 1;
    let oldSequence = 100;
    const instance = new OrbiSyncInstance(
      oldSocket as unknown as WebSocket, "instance", () => ++oldSequence,
    );
    const args = { nested: { label: "queued value" } };
    try {
      instance.sendEntityCommand({ entityId: "created", operation: "spawn", args });
      instance.sendTransform({ entityId: "moving", position: { x: 2, y: 0, z: 0 } });
      args.nested.label = "caller changed this later";
      const replacement = new FakeWs();
      let replacementSequence = 2; // ClientHello and ResumeSession already sent.
      instance._updateWebSocket(replacement as unknown as WebSocket);
      instance._updateNextSequence(() => ++replacementSequence);
      instance._flush();
      const frames = replacement.sent.map(decodeEnvelope);
      assert.deepEqual(frames.map((frame) => frame.sequence), [3n, 4n]);
      assert.equal(frames[0]!.payload.case, "entityCommand");
      if (frames[0]!.payload.case === "entityCommand") {
        assert.deepEqual(frames[0]!.payload.value.arguments, { nested: { label: "queued value" } });
      }
      assert.equal(oldSocket.sent.length, 0);
    } finally {
      instance._clearFlushTimer();
    }
  });

  it("retains sends on a closed socket until the replacement session is connected", async () => {
    const socket = new FakeWs();
    const connection = makeConnectionWithFakeWs(socket);
    const methods = connection as unknown as { setConnectionPhase(phase: string): void };
    const instance = makeInstance(socket);
    instance._setConnection(connection);
    try {
      socket.readyState = 3;
      methods.setConnectionPhase("reconnecting");
      instance.sendDomainEvent({ eventType: "queued" });
      instance.sendTransform({ entityId: "entity", position: { x: 1, y: 0, z: 0 } });
      assert.equal(socket.sent.length, 0);
      const replacement = new FakeWs();
      instance._updateWebSocket(replacement as unknown as WebSocket);
      instance._flush();
      assert.equal(replacement.sent.length, 0, "OPEN alone does not mean the session resumed");
      methods.setConnectionPhase("connected");
      connection._snapshotApplied(1n);
      instance._flush();
      assert.deepEqual(replacement.sent.map(decodeEnvelope).map((frame) => frame.sequence), [1n, 2n]);
      assert.equal(instance._hasFlushTimer(), false);
    } finally {
      instance._clearFlushTimer();
      await connection.disconnect();
    }
  });

  it("keeps both queues after a send exception and retries on a replacement socket", () => {
    class FailingSocket extends FakeWs {
      override send(): void { throw new Error("send failed"); }
    }
    const socket = new FailingSocket();
    socket.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD;
    const instance = makeInstance(socket);
    try {
      instance.sendDomainEvent({ eventType: "queued" });
      instance.sendTransform({ entityId: "entity", position: { x: 1, y: 0, z: 0 } });
      socket.bufferedAmount = 0;
      instance._flush();
      assert.equal(socket.readyState, 3, "a consumed sequence requires a fresh connection");
      assert.deepEqual(instance._getQueueLengths(), { reliable: 1, latestWins: 1, control: 0 });
      const replacement = new FakeWs();
      let sequence = 2;
      instance._updateWebSocket(replacement as unknown as WebSocket);
      instance._updateNextSequence(() => ++sequence);
      instance._flush();
      assert.deepEqual(replacement.sent.map(decodeEnvelope).map((frame) => frame.sequence), [3n, 4n]);
    } finally {
      instance._clearFlushTimer();
    }
  });

  it("rejects invalid queued payloads synchronously without consuming sequence numbers", () => {
    const socket = new FakeWs();
    socket.bufferedAmount = SEND_QUEUE_BUFFERED_THRESHOLD;
    const instance = makeInstance(socket);
    try {
      assert.throws(() => instance.sendTransform({
        entityId: "bad", position: { x: 1e100, y: 0, z: 0 },
      }));
      assert.throws(() => instance.sendTransform({
        entityId: "bad", position: { x: 0, y: 0, z: 0 }, expectedRevision: -1n,
      }));
      assert.throws(() => instance.sendEntityCommand({
        entityId: "bad", operation: "delete", expectedRevision: -1n,
      }));
      assert.deepEqual(instance._getQueueLengths(), { reliable: 0, latestWins: 0, control: 0 });
      socket.bufferedAmount = 0;
      instance.sendDomainEvent({ eventType: "valid" });
      assert.equal(decodeEnvelope(socket.sent[0]!).sequence, 1n);
    } finally {
      instance._clearFlushTimer();
    }
  });
});
