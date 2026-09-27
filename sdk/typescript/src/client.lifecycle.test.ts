import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { decodeEnvelope, encodeEnvelope, OrbiSyncClient, OrbiSyncConnection, RefreshFailedError } from "./client.js";

type Listener = (event: Event) => void;

class FakeWebSocket {
  static readonly OPEN = 1;
  static readonly CONNECTING = 0;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;
  readyState = FakeWebSocket.OPEN;
  closeCalls = 0;
  terminateCalls = 0;
  private serverSequence = 1n;
  private readonly listeners = new Map<string, Set<Listener>>();

  addEventListener(type: string, listener: Listener): void {
    let set = this.listeners.get(type);
    if (!set) {
      set = new Set();
      this.listeners.set(type, set);
    }
    set.add(listener);
  }

  removeEventListener(type: string, listener: Listener): void {
    this.listeners.get(type)?.delete(listener);
  }

  emit(type: string, event: Event = new Event(type)): void {
    // Most fixtures request ordinary wire ordering with sequence=0. Tests of
    // duplicates/gaps supply an explicit nonzero sequence instead.
    if (type === "message" && (event as MessageEvent).data instanceof Uint8Array) {
      const envelope = decodeEnvelope((event as MessageEvent).data);
      if (envelope.sequence === 0n) {
        envelope.sequence = envelope.payload.case === "serverHello" ? 1n : ++this.serverSequence;
        event = { data: encodeEnvelope(envelope) } as unknown as Event;
      }
    }
    for (const listener of this.listeners.get(type) ?? []) listener(event);
  }

  send(): void {}

  close(code = 1000, reason = ""): void {
    this.closeCalls += 1;
    this.readyState = FakeWebSocket.CLOSED;
    this.emit("close", { code, reason } as unknown as Event);
  }

  terminate(): void {
    this.terminateCalls += 1;
    this.readyState = FakeWebSocket.CLOSED;
  }

  listenerCount(type: string): number {
    return this.listeners.get(type)?.size ?? 0;
  }
}

class SilentCloseWebSocket extends FakeWebSocket {
  override close(code = 1000, reason = ""): void {
    this.closeCalls += 1;
    this.readyState = FakeWebSocket.CLOSED;
    void code;
    void reason;
  }
}

function frame(payload: unknown, sequence = 0n): Uint8Array {
  return encodeEnvelope(
    create(EnvelopeSchema, {
      protocolMajor: 1,
      protocolMinor: 0,
      messageId: "test-message",
      sequence,
      sentAtUnixMs: 1n,
      instanceId: "",
      payload: payload as never,
    }),
  );
}

function serverHelloFrame(connectionId: string): Uint8Array {
  return frame({
    case: "serverHello",
    value: {
      negotiatedMinor: 0,
      connectionId,
      heartbeatIntervalMs: 20_000n,
      serverTimeUnixMs: 1n,
      negotiatedCompression: "",
      enabledFeatures: [],
    },
  });
}

function resumeAcceptedFrame(): Uint8Array {
  return frame({
    case: "resumeAccepted",
    value: { resumeToken: "rotated-token", currentRevision: 1n },
  });
}

function makeConnection(ws: FakeWebSocket): OrbiSyncConnection {
  const hello = {
    negotiatedMinor: 0,
    connectionId: "conn-1",
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

function assertTemporaryListenersRemoved(ws: FakeWebSocket): void {
  assert.equal(ws.listenerCount("message"), 0);
  assert.equal(ws.listenerCount("error"), 0);
  assert.equal(ws.listenerCount("close"), 0);
}

describe("WebSocket candidate lifecycle", () => {
  it("ignores duplicate server sequences and reconnects on a gap before applying it", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    const internal = connection as unknown as { scheduleReconnect(): void };
    let reconnects = 0;
    internal.scheduleReconnect = () => { reconnects++; };
    try {
      const joining = connection.join("instance-1");
      socket.emit("message", { data: frame({ case: "joinAccepted", value: { instanceId: "instance-1", instanceRevision: 1n } }, 2n) } as unknown as Event);
      const instance = await joining;
      let delivered = 0;
      const errors: unknown[] = [];
      instance.on("domainEvent", () => delivered++);
      instance.on("error", error => errors.push(error));
      await new Promise(resolve => setTimeout(resolve, 1));
      socket.emit("message", { data: frame({ case: "domainEvent", value: { eventId: "first", instanceRevision: 2n } }, 3n) } as unknown as Event);
      socket.emit("message", { data: frame({ case: "domainEvent", value: { eventId: "duplicate-sequence", instanceRevision: 3n } }, 3n) } as unknown as Event);
      socket.emit("message", { data: frame({ case: "domainEvent", value: { eventId: "gap", instanceRevision: 100n } }, 5n) } as unknown as Event);
      assert.equal(delivered, 1);
      assert.equal(connection.getConnectionState().lastAppliedRevision, 2n);
      assert.equal(reconnects, 1);
      assert.equal((errors[0] as { code: string }).code, "SEQUENCE_GAP");
    } finally { await connection.disconnect(); }
  });
  it("join rejection retains its machine-readable code and allows retry on the same socket", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    try {
      const joining = connection.join("missing");
      const rejection = assert.rejects(joining, error => error instanceof Error && (error as Error & { code: string }).code === "NOT_FOUND");
      socket.emit("message", { data: frame({ case: "error", value: { code: "NOT_FOUND", message: "instance not found", retryable: false } }) } as unknown as Event);
      await rejection;
      assert.equal(socket.readyState, FakeWebSocket.OPEN);
      const retry = connection.join("valid");
      socket.emit("message", { data: frame({ case: "joinAccepted", value: { instanceId: "valid" } }) } as unknown as Event);
      assert.equal((await retry).getJoinInfo().instanceId, "valid");
    } finally { await connection.disconnect(); }
  });
  it("disconnect ends instance handles and rejects attempts to reuse the connection", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    const joining = connection.join("instance-1");
    socket.emit("message", { data: frame({ case: "joinAccepted", value: { instanceId: "instance-1" } }) } as unknown as Event);
    const instance = await joining;
    await assert.rejects(connection.join("instance-2"), /already joined/);
    await connection.disconnect();
    assert.throws(() => instance.sendDomainEvent({ eventType: "custom.chat" }), /closed/);
    await assert.rejects(connection.join("instance-1"), /closed/);
    assertTemporaryListenersRemoved(socket);
  });
  it("stops automatic resume after authentication refresh fails", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    const internal = connection as unknown as {
      client: OrbiSyncClient; doReconnect(): Promise<void>; scheduleReconnect(): void;
    };
    let retries = 0;
    internal.client.ensureValidAccessToken = async () => { throw new RefreshFailedError(new Error("revoked")); };
    internal.scheduleReconnect = () => { retries++; };
    await internal.doReconnect();
    assert.equal(retries, 0);
    assert.equal(connection.getConnectionState().phase, "offline");
    await connection.disconnect();
  });
  it("delivers a coalesced join, snapshot chunks and delta after join handlers are installed", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    try {
      const joining = connection.join("instance-1");
      socket.emit("message", { data: frame({ case: "joinAccepted", value: {
        instanceId: "instance-1", instanceRevision: 5n,
        nearbyEntities: [{ entityId: "entity-1", revision: 3n }],
      } }) } as unknown as Event);
      for (let chunkIndex = 0; chunkIndex < 2; chunkIndex++) {
        socket.emit("message", { data: frame({ case: "snapshot", value: {
          snapshotId: "snapshot-1", chunkIndex, chunkCount: 2, instanceRevision: 5n,
          data: new TextEncoder().encode(chunkIndex === 0 ? '{"entities":[' : '{"entity_id":"entity-1","revision":3}]}'),
        } }) } as unknown as Event);
      }
      socket.emit("message", { data: frame({ case: "stateDelta", value: {
        fromRevision: 5n, toRevision: 6n,
        entities: [{ entityId: "entity-1", revision: 4n }],
      } }) } as unknown as Event);
      const instance = await joining;
      const received: string[] = [];
      instance.on("joinAccepted", () => received.push("joinAccepted"));
      instance.on("snapshot", (value) => received.push(`snapshot:${(value as { chunkIndex: number }).chunkIndex}`));
      instance.on("entityUpdated", () => received.push("entityUpdated"));
      await new Promise((resolve) => setTimeout(resolve, 10));
      assert.deepEqual(received, ["joinAccepted", "snapshot:0", "snapshot:1", "entityUpdated"]);
      assert.equal(instance.getJoinInfo().instanceRevision, 5n);
      assert.equal(connection.getConnectionState().lastAppliedRevision, 6n);
    } finally {
      await connection.disconnect();
    }
  });

  it("captures an immediate join response and cancels buffered delivery on disconnect", async () => {
    class ImmediateJoinSocket extends FakeWebSocket {
      override send(): void {
        this.emit("message", { data: frame({ case: "joinAccepted", value: {
          instanceId: "instance-1", instanceRevision: 1n,
        } }) } as unknown as Event);
        this.emit("message", { data: frame({ case: "snapshot", value: {
          snapshotId: "initial", chunkCount: 1, instanceRevision: 1n,
        } }) } as unknown as Event);
      }
    }
    const socket = new ImmediateJoinSocket();
    const connection = makeConnection(socket);
    try {
      const instance = await connection.join("instance-1");
      assert.equal(socket.listenerCount("message"), 1, "temporary join listener is removed");
      let snapshots = 0;
      instance.on("snapshot", () => snapshots++);
      await connection.disconnect();
      await new Promise((resolve) => setTimeout(resolve, 10));
      assert.equal(snapshots, 0);
      assertTemporaryListenersRemoved(socket);
    } finally {
      await connection.disconnect();
    }
  });

  it("continues buffered snapshot delivery when an application listener throws", async () => {
    const socket = new FakeWebSocket();
    const connection = makeConnection(socket);
    const errors: unknown[] = [];
    try {
      const joining = connection.join("instance-1");
      socket.emit("message", { data: frame({ case: "joinAccepted", value: {
        instanceId: "instance-1", instanceRevision: 1n,
      } }) } as unknown as Event);
      socket.emit("message", { data: frame({ case: "snapshot", value: {
        snapshotId: "initial", chunkCount: 1, instanceRevision: 1n,
        data: new TextEncoder().encode('{"entities":[]}'),
      } }) } as unknown as Event);
      const instance = await joining;
      instance.on("error", error => errors.push(error));
      instance.on("snapshot", () => { throw new Error("application failed"); });
      let delivered = 0;
      instance.on("snapshot", () => delivered++);
      await new Promise((resolve) => setTimeout(resolve, 10));
      assert.equal(delivered, 1);
      assert.equal(errors.length, 1);
    } finally {
      await connection.disconnect();
    }
  });

  it("closes and terminates a candidate on open timeout", async () => {
    const originalWebSocket = globalThis.WebSocket;
    const originalSetTimeout = globalThis.setTimeout;
    const originalFetch = globalThis.fetch;
    class NeverOpenWebSocket extends FakeWebSocket {
      static readonly OPEN = FakeWebSocket.OPEN;
      static readonly CONNECTING = FakeWebSocket.CONNECTING;
      static readonly CLOSING = FakeWebSocket.CLOSING;
      static readonly CLOSED = FakeWebSocket.CLOSED;
      constructor() {
        super();
        this.readyState = NeverOpenWebSocket.CONNECTING;
      }
    }
    let candidate: NeverOpenWebSocket | null = null;
    const WebSocketConstructor = class extends NeverOpenWebSocket {
      constructor(...args: ConstructorParameters<typeof WebSocket>) {
        super();
        void args;
        candidate = this;
      }
    };
    globalThis.WebSocket = WebSocketConstructor as unknown as typeof WebSocket;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    globalThis.setTimeout = ((handler: any, timeout?: number, ...args: any[]) =>
      originalSetTimeout(handler, timeout === 5_000 ? 1 : timeout, ...args)) as typeof setTimeout;
    globalThis.fetch = (async () => ({
      ok: true,
      status: 200,
      json: async () => ({ realtime_ticket: "ticket-1", expires_in: 60 }),
    })) as unknown as typeof fetch;
    try {
      const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
      client._setTokens("access-token");
      await assert.rejects(client.connect(), /WebSocket open timeout/);
      const createdCandidate = candidate as unknown as NeverOpenWebSocket;
      assert.ok(createdCandidate);
      assert.equal(createdCandidate.closeCalls, 1);
      assert.equal(createdCandidate.terminateCalls, 1);
      assertTemporaryListenersRemoved(createdCandidate);
    } finally {
      globalThis.WebSocket = originalWebSocket;
      globalThis.setTimeout = originalSetTimeout;
      globalThis.fetch = originalFetch;
    }
  });

  it("rejects and cleans up ServerHello waiters on close and error", async () => {
    const connection = makeConnection(new FakeWebSocket());
    const methods = connection as unknown as {
      awaitServerHello(ws: WebSocket, timeoutMs: number): Promise<unknown>;
      awaitResumeResult(ws: WebSocket, timeoutMs: number): Promise<unknown>;
    };

    const closed = new FakeWebSocket();
    const helloWait = methods.awaitServerHello(closed as unknown as WebSocket, 1_000);
    closed.emit("close", { code: 1006 } as unknown as Event);
    await assert.rejects(helloWait, /closed while waiting for ServerHello/);
    assertTemporaryListenersRemoved(closed);

    const helloErrored = new FakeWebSocket();
    const helloErrorWait = methods.awaitServerHello(helloErrored as unknown as WebSocket, 1_000);
    helloErrored.emit("error");
    await assert.rejects(helloErrorWait, /error while waiting for ServerHello/);
    assertTemporaryListenersRemoved(helloErrored);

    const helloTimeout = new FakeWebSocket();
    await assert.rejects(methods.awaitServerHello(helloTimeout as unknown as WebSocket, 1), /ServerHello timeout/);
    assertTemporaryListenersRemoved(helloTimeout);

    const helloSucceeded = new FakeWebSocket();
    const helloSuccessWait = methods.awaitServerHello(helloSucceeded as unknown as WebSocket, 1_000);
    helloSucceeded.emit("message", { data: serverHelloFrame("hello-success") } as unknown as Event);
    await helloSuccessWait;
    assertTemporaryListenersRemoved(helloSucceeded);

    const errored = new FakeWebSocket();
    const resumeWait = methods.awaitResumeResult(errored as unknown as WebSocket, 1_000);
    errored.emit("error");
    await assert.rejects(resumeWait, /error while waiting for ResumeResult/);
    assertTemporaryListenersRemoved(errored);

    const resumeClosed = new FakeWebSocket();
    const resumeCloseWait = methods.awaitResumeResult(resumeClosed as unknown as WebSocket, 1_000);
    resumeClosed.emit("close", { code: 1006 } as unknown as Event);
    await assert.rejects(resumeCloseWait, /closed while waiting for ResumeResult/);
    assertTemporaryListenersRemoved(resumeClosed);

    const resumeTimeout = new FakeWebSocket();
    await assert.rejects(methods.awaitResumeResult(resumeTimeout as unknown as WebSocket, 1), /Resume result timeout/);
    assertTemporaryListenersRemoved(resumeTimeout);

    const resumeSucceeded = new FakeWebSocket();
    const resumeSuccessWait = methods.awaitResumeResult(resumeSucceeded as unknown as WebSocket, 1_000);
    resumeSucceeded.emit("message", { data: resumeAcceptedFrame() } as unknown as Event);
    await resumeSuccessWait;
    assertTemporaryListenersRemoved(resumeSucceeded);

    await connection.disconnect();
  });

  it("cleans up on abort and ignores events from the old socket after a swap", async () => {
    const oldSocket = new FakeWebSocket();
    const connection = makeConnection(oldSocket);
    const methods = connection as unknown as {
      awaitResumeResult(ws: WebSocket, timeoutMs: number, signal?: AbortSignal): Promise<unknown>;
      swapSocket(ws: WebSocket, hello: unknown): void;
      reconnectTimer: ReturnType<typeof setTimeout> | null;
      resumeToken: string;
    };

    const candidate = new FakeWebSocket();
    const controller = new AbortController();
    const resumeWait = methods.awaitResumeResult(candidate as unknown as WebSocket, 1_000, controller.signal);
    controller.abort();
    await assert.rejects(resumeWait, /lifecycle aborted/);
    assertTemporaryListenersRemoved(candidate);

    methods.swapSocket.call(connection, candidate as unknown as WebSocket, {
      negotiatedMinor: 0,
      connectionId: "conn-2",
      heartbeatIntervalMs: 20_000n,
    });
    methods.resumeToken = "before-late-event";
    oldSocket.emit("message", { data: resumeAcceptedFrame() } as unknown as Event);
    oldSocket.emit("close", { code: 1011 } as unknown as Event);
    oldSocket.emit("error");
    assert.equal(methods.reconnectTimer, null);
    assert.equal(connection._getResumeToken(), "before-late-event");
    assertTemporaryListenersRemoved(oldSocket);
    assert.equal(candidate.listenerCount("message"), 1);
    assert.equal(candidate.listenerCount("close"), 1);
    assert.equal(candidate.listenerCount("error"), 1);

    await connection.disconnect();
    assertTemporaryListenersRemoved(candidate);
  });

  it("aborts a join waiter immediately when disconnect does not emit close", async () => {
    const socket = new SilentCloseWebSocket();
    const connection = makeConnection(socket);
    const methods = connection as unknown as {
      awaitJoinAccepted(timeoutMs: number): Promise<unknown>;
    };

    const joinWait = methods.awaitJoinAccepted(60_000);
    assert.equal(socket.listenerCount("message"), 2);
    assert.equal(socket.listenerCount("error"), 2);
    assert.equal(socket.listenerCount("close"), 2);

    await connection.disconnect();
    await assert.rejects(joinWait, /lifecycle aborted/);
    assertTemporaryListenersRemoved(socket);
  });

  it("aborts a join waiter before swapping a socket that does not emit close", async () => {
    const oldSocket = new SilentCloseWebSocket();
    const connection = makeConnection(oldSocket);
    const methods = connection as unknown as {
      awaitJoinAccepted(timeoutMs: number): Promise<unknown>;
      swapSocket(ws: WebSocket, hello: unknown): void;
    };
    const joinWait = methods.awaitJoinAccepted(60_000);
    const candidate = new FakeWebSocket();

    methods.swapSocket(candidate as unknown as WebSocket, {
      negotiatedMinor: 0,
      connectionId: "conn-2",
      heartbeatIntervalMs: 20_000n,
    });

    await assert.rejects(joinWait, /replaced while waiting for JoinAccepted/);
    assertTemporaryListenersRemoved(oldSocket);
    await connection.disconnect();
    assertTemporaryListenersRemoved(candidate);
  });

  it("deduplicates concurrent connect calls into a single connection", async () => {
    const originalWebSocket = globalThis.WebSocket;
    const originalFetch = globalThis.fetch;
    const created: FakeWebSocket[] = [];
    class HelloWebSocket extends FakeWebSocket {
      static readonly OPEN = FakeWebSocket.OPEN;
      static readonly CONNECTING = FakeWebSocket.CONNECTING;
      static readonly CLOSING = FakeWebSocket.CLOSING;
      static readonly CLOSED = FakeWebSocket.CLOSED;
      constructor() {
        super();
        this.readyState = HelloWebSocket.CONNECTING;
        created.push(this);
        queueMicrotask(() => {
          this.readyState = HelloWebSocket.OPEN;
          this.emit("open");
        });
      }

      override send(data?: unknown): void {
        const payload = decodeEnvelope(data as Uint8Array).payload?.case;
        if (payload === "clientHello") {
          queueMicrotask(() =>
            this.emit("message", { data: serverHelloFrame(`conn-${created.length}`) } as unknown as Event),
          );
        }
      }
    }

    globalThis.WebSocket = HelloWebSocket as unknown as typeof WebSocket;
    globalThis.fetch = (async () => ({
      ok: true,
      status: 200,
      json: async () => ({ realtime_ticket: "ticket-1", expires_in: 60 }),
    })) as unknown as typeof fetch;
    let connection: OrbiSyncConnection | null = null;
    try {
      const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
      client._setTokens("access-token");
      const [first, second] = await Promise.all([client.connect(), client.connect()]);
      connection = first;
      assert.equal(first, second);
      assert.equal(created.length, 1);
      assert.equal(created[0]!.listenerCount("message"), 1);
      assert.equal(created[0]!.listenerCount("close"), 1);
      assert.equal(created[0]!.listenerCount("error"), 1);
    } finally {
      globalThis.WebSocket = originalWebSocket;
      globalThis.fetch = originalFetch;
      await connection?.disconnect();
    }
    assertTemporaryListenersRemoved(created[0]!);
  });

  it("leaves only the successful candidate live after a failed resume retry", async () => {
    const originalWebSocket = globalThis.WebSocket;
    const originalFetch = globalThis.fetch;
    const originalRandom = Math.random;
    const oldSocket = new FakeWebSocket();
    const connection = makeConnection(oldSocket);
    const fields = connection as unknown as {
      doReconnect(): Promise<void>;
      client: OrbiSyncClient;
      joinedInstanceId: string | null;
      resumeToken: string;
    };
    fields.client._setTokens("access-token");
    fields.joinedInstanceId = "instance-1";
    fields.resumeToken = "resume-token";

    class RetryWebSocket extends FakeWebSocket {
      static readonly instances: RetryWebSocket[] = [];

      constructor() {
        super();
        this.readyState = RetryWebSocket.CONNECTING;
        RetryWebSocket.instances.push(this);
        queueMicrotask(() => {
          this.readyState = RetryWebSocket.OPEN;
          this.emit("open");
        });
      }

      override send(data?: unknown): void {
        const payload = decodeEnvelope(data as Uint8Array).payload?.case;
        if (payload === "clientHello") {
          const connectionId = `candidate-${RetryWebSocket.instances.length}`;
          queueMicrotask(() => this.emit("message", { data: serverHelloFrame(connectionId) } as unknown as Event));
        } else if (payload === "resumeSession") {
          if (RetryWebSocket.instances.length === 1) {
            queueMicrotask(() => this.emit("close", { code: 1011, reason: "resume failed" } as unknown as Event));
          } else {
            queueMicrotask(() => {
              this.emit("message", { data: resumeAcceptedFrame() } as unknown as Event);
              this.emit("message", { data: frame({ case: "stateDelta", value: {
                fromRevision: 1n, toRevision: 2n,
              } }) } as unknown as Event);
            });
          }
        }
      }
    }

    globalThis.WebSocket = RetryWebSocket as unknown as typeof WebSocket;
    globalThis.fetch = (async () => ({
      ok: true,
      status: 200,
      json: async () => ({ realtime_ticket: "ticket-1", expires_in: 60 }),
    })) as unknown as typeof fetch;
    Math.random = () => 0;
    try {
      await fields.doReconnect();
      await new Promise((resolve) => setTimeout(resolve, 20));
      assert.equal(RetryWebSocket.instances.length, 2);
      assert.equal(
        RetryWebSocket.instances.filter((socket) => socket.readyState === RetryWebSocket.OPEN).length,
        1,
      );
      assert.equal(connection._getWs(), RetryWebSocket.instances[1] as unknown as WebSocket);
      assert.equal(connection.getConnectionState().lastAppliedRevision, 2n, "resume must not overwrite a newer replay revision");
      assertTemporaryListenersRemoved(RetryWebSocket.instances[0]!);
      assert.equal(RetryWebSocket.instances[1]!.listenerCount("message"), 1);
      assert.equal(RetryWebSocket.instances[1]!.listenerCount("close"), 1);
      assert.equal(RetryWebSocket.instances[1]!.listenerCount("error"), 1);
    } finally {
      Math.random = originalRandom;
      globalThis.WebSocket = originalWebSocket;
      globalThis.fetch = originalFetch;
      await connection.disconnect();
    }
    assertTemporaryListenersRemoved(RetryWebSocket.instances[1]!);
  });
});
