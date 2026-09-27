import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema, type Envelope, type HeartbeatAck } from "./generated/orbisync/v1/realtime_pb.js";
import {
  OrbiSyncClient,
  OrbiSyncConnection,
  createUuidV7,
  type ConnectionPhase,
  type ConnectionState,
} from "./client.js";

type Listener = (event: Event) => void;

class FakeWebSocket {
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  readyState = FakeWebSocket.OPEN;
  private readonly listeners = new Map<string, Set<Listener>>();

  addEventListener(type: string, listener: Listener): void {
    let listeners = this.listeners.get(type);
    if (!listeners) {
      listeners = new Set();
      this.listeners.set(type, listeners);
    }
    listeners.add(listener);
  }

  removeEventListener(type: string, listener: Listener): void {
    this.listeners.get(type)?.delete(listener);
  }

  emit(type: string, event: Event = new Event(type)): void {
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }

  send(): void {}

  close(code = 1000, reason = ""): void {
    this.readyState = FakeWebSocket.CLOSED;
    this.emit("close", { code, reason } as unknown as Event);
  }
}

function makeConnection(ws = new FakeWebSocket()): OrbiSyncConnection {
  const hello = {
    negotiatedMinor: 0,
    connectionId: "state-test",
    heartbeatIntervalMs: 20_000n,
    serverTimeUnixMs: BigInt(Date.now()),
    negotiatedCompression: "",
    enabledFeatures: [],
  } as never;
  const connection = new OrbiSyncConnection(
    ws as unknown as WebSocket,
    hello,
    new OrbiSyncClient({ baseUrl: "http://localhost" }),
  );
  (connection as unknown as { stopHeartbeat(): void }).stopHeartbeat();
  return connection;
}

function envelope(payload: unknown): Envelope {
  return create(EnvelopeSchema, {
    protocolMajor: 1,
    protocolMinor: 0,
    messageId: createUuidV7(),
    sequence: 1n,
    sentAtUnixMs: BigInt(Date.now()),
    instanceId: "instance-1",
    payload: payload as never,
  });
}

describe("public connection-state API", () => {
  it("supports explicit recovery after a normal remote close", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    const internal = connection as unknown as { doReconnect(): Promise<void> };
    let retries = 0;
    internal.doReconnect = async () => { retries++; };
    socket.close(1000, "server closed");
    assert.equal(connection.getConnectionState().phase, "offline");
    connection.requestReconnect();
    assert.equal(retries, 1);
    await connection.disconnect();
    assert.throws(() => connection.requestReconnect(), /closed/);
  });
  it("acknowledges applied replay instead of the advertised resume tip and never regresses on stale deltas", async () => {
    const connection = makeConnection();
    const internal = connection as unknown as { handleIncomingEnvelope(value: Envelope): void };
    internal.handleIncomingEnvelope(envelope({ case: "joinAccepted", value: { instanceRevision: 4n } }));
    internal.handleIncomingEnvelope(envelope({ case: "resumeAccepted", value: { currentRevision: 20n, resumeToken: "rotated" } }));
    assert.equal(connection.getConnectionState().lastAppliedRevision, 4n);
    internal.handleIncomingEnvelope(envelope({ case: "domainEvent", value: { instanceRevision: 6n } }));
    assert.equal(connection.getConnectionState().lastAppliedRevision, 6n);
    internal.handleIncomingEnvelope(envelope({ case: "stateDelta", value: { toRevision: 5n } }));
    assert.equal(connection.getConnectionState().lastAppliedRevision, 6n);
    connection._snapshotApplied(20n);
    assert.equal(connection.getConnectionState().lastAppliedRevision, 20n);
    await connection.disconnect();
  });
  it("immediately reports a credential-free connected snapshot and supports unsubscribe", async () => {
    const connection = makeConnection();
    const received: ConnectionState[] = [];
    const unsubscribe = connection.onConnectionStateChange((state) => received.push(state));

    assert.equal(received.length, 1);
    assert.equal(received[0]!.phase, "connected");
    assert.equal(received[0]!.reconnectAttempt, 0);
    assert.equal(received[0]!.rttMs, null);
    assert.equal(received[0]!.lastAppliedRevision, 0n);
    assert.equal("resumeToken" in received[0]!, false);
    assert.ok(Object.isFrozen(received[0]));

    unsubscribe();
    await connection.disconnect();
    assert.equal(received.length, 1);
  });

  it("reports reconnecting with the close reason and closed on explicit disconnect", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    const phases: ConnectionPhase[] = [];
    connection.onConnectionStateChange((state) => phases.push(state.phase), { emitCurrent: false });

    socket.emit("close", { code: 1011, reason: "server restart" } as unknown as Event);
    const reconnecting = connection.getConnectionState();
    assert.equal(reconnecting.phase, "reconnecting");
    assert.deepEqual(reconnecting.lastDisconnect, { code: 1011, reason: "server restart" });
    assert.equal("resumeToken" in reconnecting, false);

    await connection.disconnect();
    assert.equal(connection.getConnectionState().phase, "closed");
    assert.deepEqual(phases, ["reconnecting", "closed"]);
  });

  it("reports a normal remote close as offline", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    socket.emit("close", { code: 1000, reason: "maintenance" } as unknown as Event);

    const state = connection.getConnectionState();
    assert.equal(state.phase, "offline");
    assert.deepEqual(state.lastDisconnect, { code: 1000, reason: "maintenance" });
    await connection.disconnect();
  });

  it("publishes revision and RTT updates without exposing the resume token", async () => {
    const connection = makeConnection();
    const observed: ConnectionState[] = [];
    connection.onConnectionStateChange((state) => observed.push(state), { emitCurrent: false });
    const internals = connection as unknown as {
      handleIncomingEnvelope(value: Envelope): void;
      handleHeartbeatAck(value: HeartbeatAck): void;
      lastHeartbeatSentMs: bigint | null;
    };

    internals.handleIncomingEnvelope(
      envelope({
        case: "joinAccepted",
        value: {
          presenceId: "presence-1",
          resumeToken: "must-remain-private",
          instanceRevision: 42n,
        },
      }),
    );
    const sentAt = BigInt(Date.now() - 5);
    internals.lastHeartbeatSentMs = sentAt;
    internals.handleHeartbeatAck({
      clientTimeUnixMs: sentAt,
      serverTimeUnixMs: BigInt(Date.now()),
    } as unknown as HeartbeatAck);

    const current = connection.getConnectionState();
    assert.equal(current.lastAppliedRevision, 42n);
    assert.ok(current.rttMs !== null && current.rttMs >= 0);
    assert.equal("resumeToken" in current, false);
    assert.ok(observed.some((state) => state.lastAppliedRevision === 42n));
    assert.ok(observed.some((state) => state.rttMs !== null));
    await connection.disconnect();
  });

  it("isolates listener exceptions from state updates and other listeners", async () => {
    const connection = makeConnection();
    const originalConsoleError = console.error;
    let delivered = 0;
    console.error = () => {};
    try {
      const unsubscribeFailing = connection.onConnectionStateChange(() => {
        throw new Error("application listener failed");
      }, { emitCurrent: false });
      const unsubscribeHealthy = connection.onConnectionStateChange(() => {
        delivered += 1;
      }, { emitCurrent: false });
      const internals = connection as unknown as {
        setLastAppliedRevision(revision: bigint): void;
      };
      internals.setLastAppliedRevision(7n);
      assert.equal(delivered, 1);
      assert.equal(connection.getConnectionState().lastAppliedRevision, 7n);
      unsubscribeFailing();
      unsubscribeHealthy();
    } finally {
      console.error = originalConsoleError;
      await connection.disconnect();
    }
  });

  it("reports resyncing while a server-requested fresh join is in progress", async () => {
    const connection = makeConnection();
    const phases: ConnectionPhase[] = [];
    connection.onConnectionStateChange((state) => phases.push(state.phase), { emitCurrent: false });
    let releaseFreshJoin!: () => void;
    const freshJoin = new Promise<void>((resolve) => {
      releaseFreshJoin = resolve;
    });
    const internals = connection as unknown as {
      joinedInstanceId: string | null;
      doFreshJoin(): Promise<void>;
      handleIncomingEnvelope(value: Envelope): void;
    };
    internals.joinedInstanceId = "instance-1";
    internals.doFreshJoin = () => freshJoin;

    internals.handleIncomingEnvelope(
      envelope({ case: "resyncRequired", value: { reason: "history_gap" } }),
    );
    assert.equal(connection.getConnectionState().phase, "resyncing");
    releaseFreshJoin();
    await freshJoin;
    await new Promise<void>((resolve) => queueMicrotask(resolve));
    assert.equal(connection.getConnectionState().phase, "connected");
    assert.deepEqual(phases, ["resyncing", "connected"]);
    await connection.disconnect();
  });
});
