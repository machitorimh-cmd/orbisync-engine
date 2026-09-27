import { it } from "node:test";
import assert from "node:assert/strict";
import { inspect } from "node:util";
import { create } from "@bufbuild/protobuf";
import { EnvelopeSchema, type Envelope } from "./generated/orbisync/v1/realtime_pb.js";
import { OrbiSyncClient, OrbiSyncConnection, RealtimeError, decodeEnvelope, encodeEnvelope } from "./client.js";
import { STATE_SYNC_FEATURE } from "./sync_state.js";

class ContractSocket extends EventTarget {
  static readonly OPEN = 1;
  static readonly CLOSED = 3;
  static readonly CONNECTING = 0;
  static instances: ContractSocket[] = [];
  readyState = 0;
  bufferedAmount = 0;
  binaryType = "arraybuffer";
  sent: Envelope[] = [];
  closed: number[] = [];
  constructor(readonly url = "ws://localhost/ws", readonly protocols: string[] = []) {
    super();
    ContractSocket.instances.push(this);
    queueMicrotask(() => { this.readyState = 1; this.dispatchEvent(new Event("open")); });
  }
  receive(payload: Envelope["payload"], sequence: bigint): void {
    this.dispatchEvent(new MessageEvent("message", { data: encodeEnvelope(create(EnvelopeSchema, {
      protocolMajor: 1, protocolMinor: 0, sequence, messageId: `server-${sequence}`, payload,
    })) }));
  }
  send(bytes: Uint8Array): void {
    const envelope = decodeEnvelope(bytes);
    this.sent.push(envelope);
    if (envelope.payload.case === "clientHello") queueMicrotask(() => this.receive({ case: "serverHello", value: createHello() }, 1n));
  }
  close(code = 1000): void {
    this.closed.push(code); this.readyState = 3;
    const event = new Event("close");
    Object.assign(event, { code, reason: "test close" }); this.dispatchEvent(event);
  }
}

function createHello() {
  return create(EnvelopeSchema, { payload: { case: "serverHello", value: {
    negotiatedMinor: 0, connectionId: "connection-1", heartbeatIntervalMs: 20_000n,
    serverTimeUnixMs: 1n, negotiatedCompression: "", enabledFeatures: [],
  } } }).payload.value as import("./generated/orbisync/v1/realtime_pb.js").ServerHello;
}

it("expired login refreshes before ticket and sends the complete v1 ClientHello without logging credentials", async t => {
  t.mock.timers.enable({ apis: ["Date"], now: 1_000_000 });
  ContractSocket.instances = [];
  const originalWebSocket = globalThis.WebSocket;
  globalThis.WebSocket = ContractSocket as unknown as typeof WebSocket;
  t.after(() => { globalThis.WebSocket = originalWebSocket; });
  const output: unknown[] = [];
  for (const method of ["debug", "info", "log", "warn", "error"] as const) t.mock.method(console, method, (...args: unknown[]) => output.push(args));
  const secrets = ["private-password-867", "access-old-867", "refresh-old-867", "access-new-867", "refresh-new-867", "ticket-867"];
  const calls: string[] = [];
  t.mock.method(globalThis, "fetch", async (input: string, init: RequestInit) => {
    const pathname = new URL(input).pathname; calls.push(pathname);
    if (pathname.endsWith("/login")) {
      assert.equal(JSON.parse(init.body as string).password, secrets[0]);
      return Response.json({ access_token: secrets[1], refresh_token: secrets[2], expires_in: 60 });
    }
    if (pathname.endsWith("/refresh")) {
      assert.equal(JSON.parse(init.body as string).refresh_token, secrets[2]);
      return Response.json({ access_token: secrets[3], refresh_token: secrets[4], expires_in: 120 });
    }
    assert.equal(new Headers(init.headers).get("Authorization"), `Bearer ${secrets[3]}`);
    return Response.json({ realtime_ticket: secrets[5], expires_in: 60 });
  });
  const client = new OrbiSyncClient({ baseUrl: "http://localhost", wsPath: "/v1/realtime/ws", clientName: "game", clientVersion: "2.3.4", clientType: "mobile" });
  await client.auth.login({ loginId: "player", password: secrets[0] });
  assert.equal(client.getAuthState(), "Authenticated");
  t.mock.timers.tick(60_001);
  const start = Date.now();
  const connection = await client.connect();
  try {
    assert.deepEqual(calls, ["/v1/auth/login", "/v1/auth/refresh", "/v1/realtime/tickets"]);
    const socket = ContractSocket.instances[0];
    assert.equal(socket.url, "ws://localhost/v1/realtime/ws");
    assert.deepEqual(socket.protocols, ["orbisync.v1.protobuf"]);
    const hello = socket.sent[0];
    assert.equal(hello.sequence, 1n); assert.equal(hello.protocolMajor, 1); assert.equal(hello.protocolMinor, 0);
    assert.ok(Number(hello.sentAtUnixMs) - start < 5_000);
    assert.equal(hello.payload.case, "clientHello");
    if (hello.payload.case !== "clientHello") throw new Error("hello missing");
    const { $typeName, ...values } = hello.payload.value;
    assert.deepEqual(values, { supportedMinorMin: 0, supportedMinorMax: 0, realtimeTicket: secrets[5],
      clientName: "game", clientVersion: "2.3.4", clientType: "mobile", supportedCompressions: [], supportedFeatures: [STATE_SYNC_FEATURE], resumeToken: "" });
    assert.equal(connection.hello.negotiatedMinor, 0);
    assert.equal(connection.hello.negotiatedCompression, "");
    assert.deepEqual(connection.hello.enabledFeatures, []);
    for (const secret of secrets) assert.ok(!inspect(output, { depth: null }).includes(secret));
  } finally { await connection.disconnect(); }
});

it("real heartbeat interval and acknowledgement timers trigger recovery after three missed replies", async t => {
  t.mock.timers.enable({ apis: ["Date", "setTimeout", "setInterval"], now: 1_000_000 });
  const originalWebSocket = globalThis.WebSocket;
  globalThis.WebSocket = ContractSocket as unknown as typeof WebSocket;
  t.after(() => { globalThis.WebSocket = originalWebSocket; });
  const socket = new ContractSocket();
  socket.readyState = 1;
  const connection = new OrbiSyncConnection(socket as unknown as WebSocket, createHello(), new OrbiSyncClient({ baseUrl: "http://localhost" }));
  try {
    for (let attempt = 0; attempt < 3; attempt++) {
      t.mock.timers.tick(attempt === 0 ? 20_000 : 15_000);
      assert.equal(socket.sent.filter(frame => frame.payload.case === "heartbeat").length, attempt + 1);
      t.mock.timers.tick(4_999);
      assert.equal(socket.closed.length, 0);
      t.mock.timers.tick(1);
      if (attempt < 2) assert.equal(socket.closed.length, 0);
    }
    assert.deepEqual(socket.closed, [4000]);
    assert.equal(connection.getConnectionState().phase, "reconnecting");
  } finally { await connection.disconnect(); }
});

it("network errors do not expose login or refresh request credentials through message, stack, or cause", async t => {
  const secret = "credential-that-must-not-escape-441";
  const output: unknown[] = [];
  for (const method of ["debug", "info", "log", "warn", "error"] as const) t.mock.method(console, method, (...args: unknown[]) => output.push(args));
  t.mock.method(globalThis, "fetch", async () => { throw new Error(`network adapter echoed ${secret}`, { cause: { password: secret } }); });
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  client.on("error", value => output.push(value));
  for (const operation of [
    () => client.auth.login({ loginId: "player", password: secret }),
    () => { client._setTokens(secret, secret, -1); return client.refreshAccessToken(); },
    () => { client._setTokens(secret, secret, 60); return client.fetchRealtimeTicket(); },
  ]) {
    const result = await Promise.allSettled([operation()]);
    assert.equal(result[0].status, "rejected"); output.push(result[0]);
  }
  assert.ok(!inspect(output, { depth: null }).includes(secret), "errors must not retain raw transport causes");
});

it("protocol errors keep public fields and exclude arbitrary credential-bearing properties", () => {
  for (const code of ["INVALID_ARGUMENT", "NOT_FOUND", "INSTANCE_FULL", "RATE_LIMITED", "REVISION_MISMATCH", "ACCESS_DENIED", "AUTHENTICATION_REQUIRED", "INTERNAL_ERROR"]) {
    const error = new RealtimeError({ code, message: "request rejected", requestMessageId: "request-1", retryable: code === "RATE_LIMITED",
      password: "private-password", token: "private-token", details: { authorization: "private-token" },
    } as never);
    assert.ok(error instanceof Error); assert.equal(error.code, code);
    assert.equal(error.requestMessageId, "request-1"); assert.equal(error.retryable, code === "RATE_LIMITED");
    assert.ok(!inspect(error, { depth: null }).includes("private-"));
  }
});

it("malformed authentication responses do not retain response bytes and transport aborts remain identifiable", async t => {
  const secret = "private-response-token-662";
  const client = new OrbiSyncClient({ baseUrl: "http://localhost" });
  t.mock.method(globalThis, "fetch", async () => new Response(`{"access_token":"${secret}", BROKEN`));
  const output: unknown[] = [];
  client.on("error", error => output.push(error));
  for (const operation of [
    () => client.auth.login({ loginId: "player", password: secret }),
    () => { client._setTokens(secret, secret, -1); return client.refreshAccessToken(); },
    () => { client._setTokens(secret, secret, 60); return client.fetchRealtimeTicket(); },
  ]) {
    const result = await Promise.allSettled([operation()]);
    assert.equal(result[0].status, "rejected"); output.push(result[0]);
  }
  assert.ok(!inspect(output, { depth: null }).includes(secret));
  t.mock.method(globalThis, "fetch", async () => { throw new DOMException(secret, "AbortError"); });
  client._setTokens("access", "refresh", 60);
  await assert.rejects(client.fetch("/v1/auth/me"), error => {
    assert.equal((error as Error).name, "AbortError");
    assert.ok(!inspect(error, { depth: null }).includes(secret));
    return true;
  });
});

it("initial snapshot gates application sends and refreshes only implicit revisions before their first send", async () => {
  const socket = new ContractSocket();
  await Promise.resolve();
  const connection = new OrbiSyncConnection(socket as unknown as WebSocket, createHello(), new OrbiSyncClient({ baseUrl: "http://localhost" }));
  try {
    assert.equal(connection.getConnectionState().sessionState, "Ready");
    const joining = connection.join("room");
    socket.receive({ case: "joinAccepted", value: create(EnvelopeSchema, { payload: { case: "joinAccepted", value: {
      instanceId: "room", instanceRevision: 2n,
    } } }).payload.value as never }, 2n);
    const instance = await joining;
    assert.equal(connection.getConnectionState().sessionState, "Joining");
    const commandId = instance.sendEntityCommand({ entityId: "entity", operation: "update", args: { component_key: "state", value: 1 } });
    instance.sendEntityCommand({ entityId: "explicit", operation: "update", expectedRevision: 3n });
    instance.sendTransform({ entityId: "moving", position: { x: 1, y: 2, z: 3 } });
    const bytes = new TextEncoder().encode(JSON.stringify({ entities: [
      { entity_id: "entity", revision: 7 }, { entity_id: "explicit", revision: 8 }, { entity_id: "moving", revision: 9 },
    ] }));
    const middle = Math.floor(bytes.length / 2);
    let applied = 0;
    instance.on("snapshotApplied", () => applied++);
    await new Promise(resolve => setTimeout(resolve, 1));
    for (let index = 0; index < 2; index++) {
      socket.receive({ case: "snapshot", value: create(EnvelopeSchema, { payload: { case: "snapshot", value: {
        snapshotId: "snapshot", instanceRevision: 10n, chunkCount: 2, chunkIndex: index,
        data: index === 0 ? bytes.slice(0, middle) : bytes.slice(middle),
      } } }).payload.value as never }, BigInt(3 + index));
      if (index === 0) {
        instance._flush();
        assert.equal(applied, 0);
        assert.equal(connection.getConnectionState().sessionState, "Joining");
        assert.equal(socket.sent.length, 1, "only JoinInstance may be sent before complete snapshot");
      }
    }
    assert.equal(applied, 1);
    assert.equal(connection.getConnectionState().sessionState, "Active");
    assert.equal(connection.getConnectionState().lastAppliedRevision, 10n);
    instance._flush();
    const mutations = socket.sent.slice(1);
    assert.deepEqual(mutations.map(frame => (frame.payload.value as { expectedRevision: bigint }).expectedRevision), [7n, 3n, 9n]);
    assert.equal(mutations[0].messageId, commandId);
    // Retrying an already sent command must preserve its original payload.
    instance._setEntityRevision("entity", 11n);
    const replacement = new ContractSocket(); await Promise.resolve();
    instance._updateWebSocket(replacement as unknown as WebSocket);
    instance._flush();
    assert.equal(replacement.sent[0].messageId, commandId);
    assert.equal((replacement.sent[0].payload.value as { expectedRevision: bigint }).expectedRevision, 7n);
  } finally { await connection.disconnect(); }
});

it("missing initial snapshot times out even when no first chunk arrives", async t => {
  t.mock.timers.enable({ apis: ["setTimeout", "setInterval"] });
  const socket = new ContractSocket(); await Promise.resolve();
  const connection = new OrbiSyncConnection(socket as unknown as WebSocket, createHello(), new OrbiSyncClient({ baseUrl: "http://localhost" }));
  try {
    const joining = connection.join("room");
    socket.receive({ case: "joinAccepted", value: create(EnvelopeSchema, { payload: { case: "joinAccepted", value: { instanceId: "room" } } }).payload.value as never }, 2n);
    const instance = await joining;
    const errors: RealtimeError[] = [];
    instance.on("error", error => errors.push(error as RealtimeError));
    t.mock.timers.tick(9_999);
    assert.equal(socket.closed.length, 0);
    assert.equal(connection.getConnectionState().sessionState, "Joining");
    t.mock.timers.tick(1);
    assert.equal(errors[0].code, "INVALID_SNAPSHOT");
    assert.deepEqual(socket.closed, [4000]);
    assert.equal(connection.getConnectionState().phase, "reconnecting");
  } finally { await connection.disconnect(); }
});
