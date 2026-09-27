/**
 * OrbiSync TypeScript SDK — real protobuf wire (W-14, W-20).
 *
 * Wire types are generated from `proto/orbisync/v1/realtime.proto` via
 * `buf generate` (see `buf.gen.yaml`). This file imports those generated
 * types and uses `toBinary` / `fromBinary` for every send/recv.
 *
 * Connection state machine, backoff, resume, latest-wins queue, and heartbeat
 * follow `docs/design/client-sdk.md` §3.4 / §4.3 / §5 and MRIB.
 */

import { clone, create, fromBinary, toBinary } from "@bufbuild/protobuf";
import { InstanceSync, type ReadyOptions, type SyncOptions, type SyncStatus } from "./instance_sync.js";
import { STATE_SYNC_FEATURE, SyncError, uint64, stateBytes, type InstanceState } from "./sync_state.js";
export { SyncError, STATE_SYNC_FEATURE };
export type { InstanceState, SyncedEntity, StateValue, StateObject } from "./sync_state.js";
export type { ReadyOptions, SyncOptions, SyncStatus };
import { uuidv7 } from "./uuidv7.js";
export { uuidv7 } from "./uuidv7.js";
export { PredictedInput, RemoteInterpolator } from "./motion.js";
import {
  EnvelopeSchema,
  type Envelope,
  type ClientHello,
  type ServerHello,
  type Transform,
  type JoinInstance,
  type JoinAccepted,
  type TransformInput,
  type Heartbeat,
  type HeartbeatAck,
  type ResumeSession,
  type ResumeAccepted,
  type ResyncRequired,
  type Snapshot,
  type StateDelta,
  type EntityCommand,
  type EntityState,
  type ErrorMessage,
} from "./generated/orbisync/v1/realtime_pb.js";
import {
  parseRealtimeTicketResponse,
  RealtimeTicketRateLimitedError,
  type RealtimeTicketResponse,
} from "./realtime_ticket.js";

export { RealtimeTicketRateLimitedError };

/** Machine-readable server/SDK failure, without retaining the request or credentials. */
export class RealtimeError extends Error {
  readonly code: string;
  readonly requestMessageId: string;
  readonly retryable: boolean;
  constructor(error: Pick<ErrorMessage, "code" | "message" | "requestMessageId" | "retryable">) {
    super(error.message);
    this.name = "RealtimeError";
    this.code = error.code;
    this.requestMessageId = error.requestMessageId;
    this.retryable = error.retryable;
  }
}

function listenerFailure(event: string): RealtimeError {
  return new RealtimeError({ code: "EVENT_HANDLER_FAILED", message: `${event} listener failed`, requestMessageId: "", retryable: false });
}

// Fetch implementations and JSON parsers may attach request bodies, headers,
// or response fragments to their errors. Never propagate those raw causes.
async function fetchTransport(url: string, init: RequestInit): Promise<Response> {
  try { return await globalThis.fetch(url, init); }
  catch (error) {
    if (error instanceof Error && error.name === "AbortError") throw new DOMException("Request aborted", "AbortError");
    throw new Error("HTTP transport failed");
  }
}

async function credentialJson(response: Response): Promise<unknown> {
  try { return await response.json(); }
  catch { throw new Error("Invalid authentication response"); }
}

// ---------------------------------------------------------------------------
// Authentication methods (ADR-026)
// ---------------------------------------------------------------------------

/** An authentication method this server may accept. */
export type AuthMethod = "local" | "guest" | "name_only" | "external";

/** Every method identifier the SDK recognises. */
export const AUTH_METHODS: readonly AuthMethod[] = ["local", "guest", "name_only", "external"];

/** The token pair every authentication method returns. */
type TokenPair = {
  access_token: string;
  refresh_token?: string;
  expires_in?: number;
};

/** The server's error envelope. */
type ErrorEnvelope = { error?: { code?: string } };

export { checkpointCapacityPhase } from "./checkpoint_capacity.js";
export type { CheckpointCapacityPhase } from "./checkpoint_capacity.js";

/**
 * Thrown when the requested method is not enabled on this server.
 *
 * The server answers the same way whether the method was switched off or never
 * configured, so this says nothing about how the deployment is set up — only
 * that this route is not available and another should be tried.
 */
export class AuthMethodDisabledError extends Error {
  readonly status = 403 as const;
  readonly code = "AUTH_METHOD_DISABLED" as const;
  constructor(message = "authentication method is not enabled") {
    super(message);
    this.name = "AuthMethodDisabledError";
  }
}

// ---------------------------------------------------------------------------
// SDK-02 send-queue constants (client-sdk.md §4.3, ADR SDK-02)
// ---------------------------------------------------------------------------

/** Threshold for congestion detection: 256 KiB (see F6 §3.1). */
export const SEND_QUEUE_BUFFERED_THRESHOLD = 256 * 1024;
/** Flush interval while congested: 50 ms (20 Hz, F6 §3.2). */
export const SEND_QUEUE_FLUSH_INTERVAL_MS = 50;
/** Latest-wins per-key capacity (F6 §3.3). */
export const LATEST_WINS_MAX_KEYS = 1024;
/** Reliable queue capacity (F6 §3.4). */
export const RELIABLE_MAX_QUEUE = 256;
/** Aggregate encoded payload and latest-key budget across both send lanes. */
export const SEND_QUEUE_MAX_BYTES = 4 * 1024 * 1024;

/**
 * Generates the canonical lowercase UUIDv7 required for OrbiSync public IDs.
 * `crypto.randomUUID()` produces UUIDv4 and is not accepted by the server.
 */
export function createUuidV7(nowUnixMs = Date.now()): string {
  if (!Number.isSafeInteger(nowUnixMs) || nowUnixMs < 0 || nowUnixMs > 0xffffffffffff) {
    throw new RangeError("nowUnixMs must fit in the UUIDv7 48-bit timestamp");
  }

  const bytes = new Uint8Array(16);
  let timestamp = nowUnixMs;
  for (let index = 5; index >= 0; index -= 1) {
    bytes[index] = timestamp & 0xff;
    timestamp = Math.floor(timestamp / 256);
  }
  crypto.getRandomValues(bytes.subarray(6));
  bytes[6] = (bytes[6]! & 0x0f) | 0x70;
  bytes[8] = (bytes[8]! & 0x3f) | 0x80;

  const hex = Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
  return `${hex.slice(0, 8)}-${hex.slice(8, 12)}-${hex.slice(12, 16)}-${hex.slice(16, 20)}-${hex.slice(20)}`;
}

/** Error thrown when reliable queue is saturated (must not drop). */
export class ReliableQueueOverflowError extends Error {
  constructor(message = `reliable queue saturated (${RELIABLE_MAX_QUEUE})`) {
    super(message);
    this.name = "ReliableQueueOverflowError";
  }
}

// ---------------------------------------------------------------------------
// Re-exported protocol constants and generated types
// ---------------------------------------------------------------------------

export const PROTOCOL_MAJOR = 1;
export const WEBSOCKET_SUBPROTOCOL = "orbisync.v1.protobuf";
export const WEBSOCKET_SUBPROTOCOL_JSON_DEBUG = "orbisync.v1.json";

// Re-export generated message types so consumers can import from the SDK.
export type {
  Envelope,
  ClientHello,
  ServerHello,
  Transform,
  JoinInstance,
  JoinAccepted,
  TransformInput,
  Heartbeat,
  HeartbeatAck,
  ResumeSession,
  ResumeAccepted,
  ResyncRequired,
  Snapshot,
  StateDelta,
  EntityCommand,
  EntityState,
  ErrorMessage,
};

// ---------------------------------------------------------------------------
// Protobuf encode / decode — real wire, not stubs
// ---------------------------------------------------------------------------

/**
 * Encode an `Envelope` to protobuf binary via generated `toBinary`.
 * Uses `EnvelopeSchema` from `buf generate`; never hand-encode.
 */
export function encodeEnvelope(env: Envelope): Uint8Array {
  return toBinary(EnvelopeSchema, env);
}

/**
 * Decode protobuf binary into an `Envelope` via generated `fromBinary`.
 */
export function decodeEnvelope(bytes: Uint8Array): Envelope {
  if (bytes.byteLength > 65536) throw new SyncError("RESOURCE_LIMIT");
  return fromBinary(EnvelopeSchema, bytes);
}

/** Convert WebSocket MessageEvent data to Uint8Array (handles ArrayBuffer, Buffer, Uint8Array). */
function wsDataToBytes(data: unknown): Uint8Array {
  if (data instanceof ArrayBuffer) return new Uint8Array(data);
  if (data instanceof Uint8Array) return new Uint8Array(data.buffer, data.byteOffset, data.byteLength);
  // Node Buffer is Uint8Array subclass but check via Buffer.isBuffer for safety
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  if (typeof (globalThis as any).Buffer !== "undefined" && (globalThis as any).Buffer.isBuffer(data)) {
    const buf = data as Uint8Array;
    return new Uint8Array(buf.buffer, buf.byteOffset, buf.byteLength);
  }
  if (typeof data === "string") return new TextEncoder().encode(data as string);
  // fallback
  return new Uint8Array(data as ArrayBuffer);
}

type SocketResult = { ws: WebSocket; serverHello: ServerHello };

type SocketHandlers = {
  ws: WebSocket;
  onMessage: (event: Event) => void;
  onClose: (event: Event) => void;
  onError: (event: Event) => void;
};

/** Dispose a candidate that never became the active connection. */
function closeCandidateSocket(ws: WebSocket, reason = "candidate connection failed"): void {
  try {
    ws.close(4000, reason);
  } catch {
    // Preserve the original lifecycle error if close itself fails.
  }
  const terminate = (ws as unknown as { terminate?: () => void }).terminate;
  if (typeof terminate === "function") {
    try {
      terminate.call(ws);
    } catch {
      // Cleanup is best-effort.
    }
  }
}

function unrefTimer(timer: ReturnType<typeof setTimeout>): void {
  // eslint-disable-next-line @typescript-eslint/no-explicit-any
  (timer as any)?.unref?.();
}

function waitForSocketOpen(ws: WebSocket, timeoutMs: number, signal?: AbortSignal): Promise<void> {
  return new Promise<void>((resolve, reject) => {
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const cleanup = (): void => {
      if (timer !== null) clearTimeout(timer);
      ws.removeEventListener("open", onOpen);
      ws.removeEventListener("error", onError);
      ws.removeEventListener("close", onClose);
      signal?.removeEventListener("abort", onAbort);
    };
    const succeed = (): void => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve();
    };
    const fail = (message: string): void => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(message));
    };
    const onOpen = (): void => succeed();
    const onError = (): void => fail("WebSocket open failed");
    const onClose = (): void => fail("WebSocket closed before open");
    const onAbort = (): void => fail("WebSocket lifecycle aborted");
    if (signal?.aborted) {
      onAbort();
      return;
    }
    timer = setTimeout(() => fail("WebSocket open timeout"), timeoutMs);
    unrefTimer(timer);
    try {
      ws.addEventListener("open", onOpen);
      ws.addEventListener("error", onError);
      ws.addEventListener("close", onClose);
      signal?.addEventListener("abort", onAbort);
    } catch {
      fail("WebSocket open listener setup failed");
    }
  });
}

function waitForServerHello(
  ws: WebSocket,
  timeoutMs: number,
  signal?: AbortSignal,
  start?: () => void,
): Promise<ServerHello> {
  return new Promise<ServerHello>((resolve, reject) => {
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const cleanup = (): void => {
      if (timer !== null) clearTimeout(timer);
      ws.removeEventListener("message", onMessage);
      ws.removeEventListener("error", onError);
      ws.removeEventListener("close", onClose);
      signal?.removeEventListener("abort", onAbort);
    };
    const succeed = (hello: ServerHello): void => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(hello);
    };
    const fail = (message: string | Error): void => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(message instanceof Error ? message : new Error(message));
    };
    const onMessage = (ev: MessageEvent): void => {
      try {
        const env = decodeEnvelope(wsDataToBytes(ev.data));
        if (env.payload?.case === "serverHello") succeed(env.payload.value);
        else if (env.payload?.case === "error") fail(new RealtimeError(env.payload.value));
      } catch {
        // Ignore malformed frames until a valid response, close, or timeout.
      }
    };
    const onError = (): void => fail("WebSocket error while waiting for ServerHello");
    const onClose = (): void => fail("WebSocket closed while waiting for ServerHello");
    const onAbort = (): void => fail("WebSocket lifecycle aborted");
    if (signal?.aborted) {
      onAbort();
      return;
    }
    timer = setTimeout(() => fail("ServerHello timeout"), timeoutMs);
    unrefTimer(timer);
    try {
      ws.addEventListener("message", onMessage);
      ws.addEventListener("error", onError);
      ws.addEventListener("close", onClose);
      signal?.addEventListener("abort", onAbort);
    } catch {
      fail("ServerHello listener setup failed");
      return;
    }
    try {
      start?.();
    } catch (err) {
      fail(err instanceof Error ? err.message : "ServerHello send failed");
    }
  });
}

function waitForResumeResult(
  ws: WebSocket,
  timeoutMs: number,
  signal?: AbortSignal,
): Promise<{ case: "resumeAccepted" | "resyncRequired" | "error"; value: unknown }> {
  return new Promise((resolve, reject) => {
    let settled = false;
    let timer: ReturnType<typeof setTimeout> | null = null;
    const cleanup = (): void => {
      if (timer !== null) clearTimeout(timer);
      ws.removeEventListener("message", onMessage);
      ws.removeEventListener("error", onError);
      ws.removeEventListener("close", onClose);
      signal?.removeEventListener("abort", onAbort);
    };
    const succeed = (result: { case: "resumeAccepted" | "resyncRequired" | "error"; value: unknown }): void => {
      if (settled) return;
      settled = true;
      cleanup();
      resolve(result);
    };
    const fail = (message: string): void => {
      if (settled) return;
      settled = true;
      cleanup();
      reject(new Error(message));
    };
    const onMessage = (ev: MessageEvent): void => {
      try {
        const env = decodeEnvelope(wsDataToBytes(ev.data));
        if (env.payload?.case === "resumeAccepted") succeed({ case: "resumeAccepted", value: env.payload.value });
        else if (env.payload?.case === "resyncRequired") succeed({ case: "resyncRequired", value: env.payload.value });
        else if (env.payload?.case === "error") succeed({ case: "error", value: env.payload.value });
      } catch {
        // Ignore malformed frames until a valid response, close, or timeout.
      }
    };
    const onError = (): void => fail("WebSocket error while waiting for ResumeResult");
    const onClose = (): void => fail("WebSocket closed while waiting for ResumeResult");
    const onAbort = (): void => fail("WebSocket lifecycle aborted");
    if (signal?.aborted) {
      onAbort();
      return;
    }
    timer = setTimeout(() => fail("Resume result timeout"), timeoutMs);
    unrefTimer(timer);
    try {
      ws.addEventListener("message", onMessage);
      ws.addEventListener("error", onError);
      ws.addEventListener("close", onClose);
      signal?.addEventListener("abort", onAbort);
    } catch {
      fail("Resume result listener setup failed");
    }
  });
}

async function openCandidateSocket(args: {
  wsUrl: string;
  subprotocol: string;
  ticket: string;
  resumeToken: string;
  helloSequence: number;
  clientName: string;
  clientVersion: string;
  clientType: string;
  signal?: AbortSignal;
}): Promise<SocketResult> {
  let ws: WebSocket | null = null;
  try {
    ws = new WebSocket(args.wsUrl, [args.subprotocol]);
    ws.binaryType = "arraybuffer";
    const socket = ws;
    await waitForSocketOpen(socket, 5_000, args.signal);
    const hello: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: 0,
      messageId: createUuidV7(),
      sequence: BigInt(args.helloSequence),
      sentAtUnixMs: BigInt(Date.now()),
      instanceId: "",
      payload: {
        case: "clientHello",
        value: {
          supportedMinorMin: 0,
          supportedMinorMax: 0,
          realtimeTicket: args.ticket,
          clientName: args.clientName,
          clientVersion: args.clientVersion,
          clientType: args.clientType,
          supportedCompressions: [],
          supportedFeatures: [STATE_SYNC_FEATURE],
          resumeToken: args.resumeToken,
        },
      },
    });
    const serverHello = await waitForServerHello(socket, 5_000, args.signal, () => {
      socket.send(encodeEnvelope(hello));
    });
    return { ws: socket, serverHello };
  } catch (err) {
    if (ws !== null) closeCandidateSocket(ws);
    throw err;
  }
}

// ---------------------------------------------------------------------------
// SDK public API — per `client-sdk.md` §10
// ---------------------------------------------------------------------------

export type ClientOptions = {
  baseUrl: string;
  /** Override subprotocol for tests; default is WEBSOCKET_SUBPROTOCOL. */
  subprotocol?: string;
  /** Client identity shown in ClientHello.client_name/version. */
  clientName?: string;
  clientVersion?: string;
  clientType?: "desktop" | "mobile" | "server";
  /** Override WebSocket path; default "/ws" (server mounts /ws and /v1/realtime/ws). */
  wsPath?: string;
  sync?: SyncOptions;
};

/** Stable, public realtime lifecycle phases. */
export type ConnectionPhase = "connected" | "reconnecting" | "resyncing" | "offline" | "closed";

/** Sanitized information about the most recent transport loss. */
export type ConnectionDisconnect = Readonly<{
  code: number | null;
  reason: string | null;
}>;

/**
 * Immutable connection observability snapshot.
 *
 * Resume credentials are deliberately excluded. A new snapshot is emitted
 * when phase, reconnect attempt, RTT, or last-applied revision changes.
 */
export type ConnectionState = Readonly<{
  phase: ConnectionPhase;
  sessionState: "Ready" | "Joining" | "Active" | "Resuming" | "Closed";
  reconnectAttempt: number;
  lastDisconnect: ConnectionDisconnect | null;
  rttMs: number | null;
  lastAppliedRevision: bigint;
  phaseChangedAtUnixMs: number;
}>;

export type ConnectionStateListener = (state: ConnectionState) => void;

export type ConnectionStateSubscriptionOptions = Readonly<{
  /** Immediately deliver the current snapshot. Defaults to true. */
  emitCurrent?: boolean;
}>;

/** Public, immutable join result with resume credentials intentionally omitted. */
export type InstanceJoinInfo = Readonly<{
  presenceId: string;
  userId: string;
  instanceId: string;
  instanceRevision: bigint;
  permissions: Readonly<{
    entitySpawn: boolean;
    entityUpdateOwn: boolean;
    entityUpdateAny: boolean;
  }>;
  nearbyEntityCount: number;
  nearbyPresenceCount: number;
  serverTimeUnixMs: bigint;
}>;

type InstanceHandler<T> = (payload: T) => void;

/** A receipt confirms durability. Any retry after uncertainty must preserve the original request. */
export type InputResult =
  | { status: "accepted"; commandId: string; entityRevision: bigint; instanceRevision: bigint }
  | { status: "rejected" | "uncertain"; commandId: string; code: string; detail: string };
export type InputOptions = {
  entityId: string;
  rule: string;
  intent: Record<string, unknown>;
  expectedRevision?: number | bigint;
  commandId?: string;
  timeoutMs?: number;
};

export class OrbiSyncInstance {
  private pendingInputs = new Map<string, { settle: (result: InputResult) => void }>();
  private handlers = new Map<string, Set<InstanceHandler<unknown>>>();
  /** Last known revision per entity, stored as bigint (wire type). */
  private entityRevisions = new Map<string, bigint>();
  private connectionRef: OrbiSyncConnection | null = null;
  private sync: InstanceSync | null = null;

  get state(): InstanceState { return this.sync?.state ?? { revision: 0n, entities: new Map(), presences: new Map(), metadata: {} }; }
  /** Read one applied entity revision without cloning the complete instance state. */
  entityRevision(entityId: string): bigint | undefined {
    return this.sync?.enabled ? this.sync.entityRevision(entityId) : this.entityRevisions.get(entityId);
  }
  get syncStatus(): SyncStatus { return this.sync?.status ?? "failed"; }
  ready(options?: ReadyOptions): Promise<void> { return this.sync?.ready(options) ?? Promise.reject(new SyncError("UNSUPPORTED_SERVER")); }
  _beginSync(generation: number, supported: boolean, options?: SyncOptions): void {
    this.sync ??= new InstanceSync(this.instanceId, (event, payload) => {
      this.connectionRef?._appliedRevision(this.sync!.revision);
      this.emit(event, payload);
    }, error => this.connectionRef?._requestSyncRecovery(error), options);
    this.sync.begin(generation, supported);
  }
  _syncEnabled(): boolean { return this.sync?.enabled ?? false; }
  _stopSync(error: SyncError, status: "reconnecting" | "failed" | "closed" = "reconnecting"): void {
    this.stopFlushTimer();
    for (const [commandId, pending] of this.pendingInputs) {
      pending.settle({ status: "uncertain", commandId, code: error.code, detail: error.message });
    }
    // Queued envelopes receive their wire sequence only when sent. Preserve
    // them through reconnect; final close releases them below.
    if (status !== "reconnecting") {
      this.latestWinsQueue.clear();
      this.reliableQueue = [];
      this.unacknowledged.clear();
    }
    this.entityRevisions.clear();
    this.sync?.stop(error, status);
    if (status === "closed" || status === "failed") this.handlers.clear();
  }
  _syncFailure(error: unknown): void { this.sync?.fail(error); }
  private requireReady(): void {
    if (this.sync?.enabled && this.sync.status !== "ready") throw new SyncError("NOT_READY");
  }
  private joinInfo: InstanceJoinInfo | null = null;
  private initialEvents: Envelope[] | null = null;
  private initialDispatchTimer: ReturnType<typeof setTimeout> | null = null;
  private readonly automaticRevisions = new WeakSet<Envelope>();
  private snapshotAssembly: {
    id: string; revision: bigint; count: number; bytes: number; chunks: Map<number, Uint8Array>;
  } | null = null;
  private snapshotTimeout: ReturnType<typeof setTimeout> | null = null;
  private readonly receivedReliableIds = new Set<string>();

  /** Bounded wire hint; local dedup retains a larger window. */
  _receivedMessageIds(): string[] { return [...this.receivedReliableIds].slice(-256); }

  // -- Send queues (client-sdk.md §4.3, SDK-02) --------------------------------
  /** Keep envelopes unnumbered until sent; coalescing must not leave sequence gaps. */
  private latestWinsQueue = new Map<string, Envelope>();
  private closed = false;
  private readonly unacknowledged = new Map<string, Envelope>();
  /** Reliable queue (FIFO, bounded, no drop). */
  private reliableQueue: Envelope[] = [];
  private flushTimer: ReturnType<typeof setInterval> | null = null;
  private latestWinsDroppedUpdates = 0;
  private latestWinsCapacityDrops = 0;
  private reliableSaturationErrors = 0;

  constructor(
    private ws: WebSocket,
    private readonly instanceId: string,
    private nextSequence: () => number,
  ) {}

  /** Called by OrbiSyncConnection after join to link back for explicit close. */
  _setConnection(conn: OrbiSyncConnection): void {
    this.connectionRef = conn;
  }

  _setJoinInfo(accepted: JoinAccepted): void {
    this.entityRevisions.clear();
    for (const entity of accepted.nearbyEntities) {
      this._setEntityRevision(entity.entityId, entity.revision);
    }
    this.joinInfo = Object.freeze({
      presenceId: accepted.presenceId,
      userId: accepted.userId,
      instanceId: accepted.instanceId,
      instanceRevision: accepted.instanceRevision,
      permissions: Object.freeze({
        entitySpawn: accepted.entitySpawn,
        entityUpdateOwn: accepted.entityUpdateOwn,
        entityUpdateAny: accepted.entityUpdateAny,
      }),
      nearbyEntityCount: accepted.nearbyEntities.length,
      nearbyPresenceCount: accepted.nearbyPresenceIds.length,
      serverTimeUnixMs: accepted.serverTimeUnixMs,
    });
  }

  /** Capture frames delivered in the same socket task as JoinAccepted. */
  _beginInitialDelivery(accepted: JoinAccepted): void {
    this._setJoinInfo(accepted);
    this.initialEvents = [create(EnvelopeSchema, {
      payload: { case: "joinAccepted", value: accepted },
    })];
    this.snapshotTimeout = setTimeout(() => {
      this.snapshotTimeout = null;
      this.emit("error", new RealtimeError({ code: "INVALID_SNAPSHOT", message: "initial snapshot did not arrive", requestMessageId: "", retryable: true }));
      this.connectionRef?._forceCloseTransport(4000, "missing initial snapshot");
    }, 10_000);
    unrefTimer(this.snapshotTimeout);
  }

  _hasBufferedInitialDelivery(): boolean { return this.initialEvents !== null; }

  /** Give the caller of await join() one turn to register synchronous handlers. */
  _finishInitialDelivery(): void {
    this.initialDispatchTimer = setTimeout(() => {
      this.initialDispatchTimer = null;
      const pending = this.initialEvents ?? [];
      this.initialEvents = null;
      for (const envelope of pending) this._dispatch(envelope);
    }, 0);
  }

  _cancelInitialDelivery(): void {
    if (this.initialDispatchTimer !== null) clearTimeout(this.initialDispatchTimer);
    this.initialDispatchTimer = null;
    this.initialEvents = null;
    this.resetSnapshotAssembly();
  }

  /** Returns the server-authoritative join result without the resume token. */
  getJoinInfo(): InstanceJoinInfo {
    if (this.joinInfo === null) throw new Error("instance join has not completed");
    return this.joinInfo;
  }

  /** Update underlying WebSocket after a reconnect (W-20). Keeps queued items. */
  _updateWebSocket(ws: WebSocket): void {
    this.ensureOpen();
    this.ws = ws;
    this.resetSnapshotAssembly();
    // Preserve the original IDs and order. Server echoes/deterministic errors
    // acknowledge these messages; a socket write alone cannot do so.
    const pendingIds = new Set(this.reliableQueue.map(envelope => envelope.messageId));
    this.reliableQueue.unshift(...Array.from(this.unacknowledged.values()).filter(envelope => !pendingIds.has(envelope.messageId)));
    this.ensureFlushTimer();
  }

  /** A fresh session cannot establish whether an old transient event applied. */
  _freshSession(): void {
    for (const [id, envelope] of this.unacknowledged) {
      if (envelope.payload.case !== "domainEvent") continue;
      this.unacknowledged.delete(id);
      this.reliableQueue = this.reliableQueue.filter(item => item.messageId !== id);
      this.emit("error", { code: "DELIVERY_UNKNOWN", requestMessageId: id, retryable: false,
        message: "The previous session could not resume; this event may already have been delivered" });
    }
  }

  /** Update nextSequence callback after reconnect (sequence resets). */
  _updateNextSequence(fn: () => number): void {
    this.nextSequence = fn;
  }

  on(event: "snapshot", handler: InstanceHandler<unknown>): void;
  on(event: "entityUpdated", handler: InstanceHandler<unknown>): void;
  on(event: "domainEvent", handler: InstanceHandler<unknown>): void;
  on(event: string, handler: InstanceHandler<unknown>): void;
  on(event: string, handler: InstanceHandler<unknown>): void {
    if (event.length > 256) throw new SyncError("RESOURCE_LIMIT");
    let set = this.handlers.get(event);
    if (!set?.has(handler) && [...this.handlers.values()].reduce((count, handlers) => count + handlers.size, 0) >= 1024) throw new SyncError("RESOURCE_LIMIT");
    if (!set) {
      set = new Set();
      this.handlers.set(event, set);
    }
    set.add(handler);
  }

  off(event: string, handler: InstanceHandler<unknown>): void {
    const handlers = this.handlers.get(event);
    handlers?.delete(handler);
    if (handlers?.size === 0) this.handlers.delete(event);
  }

  // -- Queue helpers -----------------------------------------------------------

  private getBufferedAmount(): number {
    // Fallback 0 when bufferedAmount unavailable → treat as not congested (immediate send).
    // This degrades to pre-queue behaviour (no coalescing) but never worsens; it is intentional per review.
    return (this.ws as unknown as { bufferedAmount?: number }).bufferedAmount ?? 0;
  }

  private isCongested(): boolean {
    return this.getBufferedAmount() >= SEND_QUEUE_BUFFERED_THRESHOLD;
  }

  private canSend(): boolean {
    return !this.closed && this.ws.readyState === 1
      && !this.isCongested()
      && (this.connectionRef === null || this.connectionRef._canSendInstanceMessages());
  }

  private ensureFlushTimer(): void {
    if (this.flushTimer !== null) return;
    if (this.latestWinsQueue.size === 0 && this.reliableQueue.length === 0) return;
    this.flushTimer = setInterval(() => this.flush(), SEND_QUEUE_FLUSH_INTERVAL_MS);
    // Unref so timer does not keep Node process alive in tests.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (this.flushTimer as any)?.unref?.();
  }

  private stopFlushTimer(): void {
    if (this.flushTimer !== null) {
      clearInterval(this.flushTimer);
      this.flushTimer = null;
    }
  }

  private stopFlushTimerIfIdle(): void {
    if (this.flushTimer !== null && this.latestWinsQueue.size === 0 && this.reliableQueue.length === 0) {
      this.stopFlushTimer();
    }
  }

  /** Flush queued items when not congested. Public for tests. */
  _flush(): void {
    this.flush();
  }

  private sendEnvelope(envelope: Envelope): void {
    // Resolve implicit revisions after initial sync/resume, once before the
    // first send. Retries keep the original payload for server deduplication.
    if (this.automaticRevisions.delete(envelope)
        && (envelope.payload.case === "entityCommand" || envelope.payload.case === "transformInput")) {
      envelope.payload.value.expectedRevision = this.entityRevisions.get(envelope.payload.value.entityId) ?? 0n;
    }
    envelope.sequence = BigInt(this.nextSequence());
    envelope.sentAtUnixMs = BigInt(Date.now());
    const bytes = encodeEnvelope(envelope);
    if (envelope.payload.case === "entityCommand" || envelope.payload.case === "domainEvent") {
      this.unacknowledged.set(envelope.messageId, envelope);
    }
    try {
      this.ws.send(bytes);
    } catch (error) {
      // A reserved sequence cannot be skipped on a live connection. Reconnect
      // before retrying pending messages with the new connection's counter.
      closeCandidateSocket(this.ws, "queued send failed");
      throw error;
    }
  }

  private flush(): void {
    // Keep pending messages until both the transport and joined session are ready.
    if (!this.canSend()) return;
    // Drain reliable first (must not drop).
    while (this.reliableQueue.length > 0) {
      if (!this.canSend()) break;
      try {
        const next = this.reliableQueue[0]!;
        this.sendEnvelope(next);
        // An immediate acknowledgement may already have removed this entry.
        const index = this.reliableQueue.indexOf(next);
        if (index !== -1) this.reliableQueue.splice(index, 1);
      } catch {
        // Keep the head and do not let latest-wins messages pass a failed send.
        return;
      }
    }
    for (const [key, envelope] of this.latestWinsQueue) {
      if (!this.canSend()) break;
      try {
        this.sendEnvelope(envelope);
        this.latestWinsQueue.delete(key);
      } catch {
        return;
      }
    }
    this.stopFlushTimerIfIdle();
  }

  private sendQueueBytes(): number {
    return this.reliableQueue.reduce((sum, item) => sum + encodeEnvelope(item).byteLength, 0)
      + [...this.latestWinsQueue].reduce((sum, [key, item]) => sum + key.length * 2 + encodeEnvelope(item).byteLength, 0);
  }

  private enqueueReliable(envelope: Envelope): void {
    const outstanding = this.unacknowledged.size + this.reliableQueue.reduce(
      (count, item) => count + (this.unacknowledged.has(item.messageId) ? 0 : 1), 0);
    if (outstanding >= RELIABLE_MAX_QUEUE) {
      this.reliableSaturationErrors++;
      throw new ReliableQueueOverflowError();
    }
    if (this.canSend() && this.reliableQueue.length === 0 && this.latestWinsQueue.size === 0) {
      this.sendEnvelope(envelope);
      return;
    }
    if (this.canSend()) {
      // Try to drain pending before queuing new reliable
      this.flush();
      if (this.canSend() && this.reliableQueue.length === 0 && this.latestWinsQueue.size === 0) {
        this.sendEnvelope(envelope);
        return;
      }
    }
    if (this.reliableQueue.length >= RELIABLE_MAX_QUEUE) {
      this.reliableSaturationErrors++;
      throw new ReliableQueueOverflowError();
    }
    if (this.sendQueueBytes() + encodeEnvelope(envelope).byteLength > SEND_QUEUE_MAX_BYTES) throw new SyncError("RESOURCE_LIMIT");
    this.reliableQueue.push(envelope);
    this.ensureFlushTimer();
  }

  // -- Queue metrics (F6 §3.6) -------------------------------------------------

  /** Observable counters for F6 §3.6 — readable for debugging / tests. */
  getQueueMetrics(): {
    latestWins: { queueLength: number; droppedUpdates: number; capacityDrops: number };
    reliable: { queueLength: number; inFlight: number; saturationErrors: number };
    control: { queueLength: number };
  } {
    return {
      latestWins: {
        queueLength: this.latestWinsQueue.size,
        droppedUpdates: this.latestWinsDroppedUpdates,
        capacityDrops: this.latestWinsCapacityDrops,
      },
      reliable: {
        queueLength: this.reliableQueue.length,
        inFlight: this.unacknowledged.size,
        saturationErrors: this.reliableSaturationErrors,
      },
      control: { queueLength: 0 },
    };
  }

  /** Alias for tests: current queue lengths per type. */
  _getQueueLengths(): { latestWins: number; reliable: number; control: number } {
    const m = this.getQueueMetrics();
    return { latestWins: m.latestWins.queueLength, reliable: m.reliable.queueLength, control: m.control.queueLength };
  }

  _getDroppedUpdates(): number {
    return this.latestWinsDroppedUpdates;
  }

  _getCapacityDrops(): number {
    return this.latestWinsCapacityDrops;
  }

  _getReliableSaturationErrors(): number {
    return this.reliableSaturationErrors;
  }

  _hasFlushTimer(): boolean {
    return this.flushTimer !== null;
  }

  _clearFlushTimer(): void {
    this.stopFlushTimer();
  }

  /** Permanently ends this instance handle; transport recovery does not call this. */
  _close(): void {
    this.closed = true;
    this.stopFlushTimer();
    this._cancelInitialDelivery();
    this.resetSnapshotAssembly();
    this.latestWinsQueue.clear();
    this.reliableQueue.length = 0;
    this.unacknowledged.clear();
    this.handlers.clear();
  }

  private ensureOpen(): void {
    if (this.closed) throw new Error("instance is closed");
  }

  /** Seeds revisions included in JoinAccepted before this instance exists. */
  _setEntityRevision(entityId: string, revision: number | bigint): void {
    this.entityRevisions.set(entityId, BigInt(revision));
  }

  /** Latest-wins send queue coalesces per (entity_id, component) before ws.send (client-sdk.md §4.3, SDK-02). */
  sendTransform(opts: {
    entityId: string;
    position: { x: number; y: number; z: number };
    rotation?: { x: number; y: number; z: number; w: number };
    expectedRevision?: number | bigint;
    /** Component discriminant for key (entity_id, component). Defaults to "transform". */
    component?: string;
  }): void {
    this.ensureOpen();
    this.requireReady();
    if (!opts.entityId || opts.entityId.length > 256 || (opts.component?.length ?? 0) > 256) throw new SyncError("INVALID_UPDATE");
    const component = (opts as { component?: string }).component ?? "transform";
    const key = `${opts.entityId}:${component}`;
    const stored = this.sync?.enabled ? this.sync.entityRevision(opts.entityId) : this.entityRevisions.get(opts.entityId);
    const expectedRevision: bigint =
      opts.expectedRevision !== undefined
        ? uint64(opts.expectedRevision, "INVALID_UPDATE")
        : stored ?? 0n;
    if (expectedRevision < 0n || expectedRevision > 0xffffffffffffffffn) {
      throw new RangeError("expectedRevision must fit in uint64");
    }
    for (const value of [
      opts.position.x, opts.position.y, opts.position.z,
      opts.rotation?.x ?? 0, opts.rotation?.y ?? 0,
      opts.rotation?.z ?? 0, opts.rotation?.w ?? 1,
    ]) {
      if (!Number.isFinite(value) || !Number.isFinite(Math.fround(value))) {
        throw new RangeError("transform components must be finite float32 values");
      }
    }
    const envelope: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: 0,
      messageId: createUuidV7(),
      instanceId: this.instanceId,
      payload: {
        case: "transformInput",
        value: {
          entityId: opts.entityId,
          expectedRevision,
          transform: {
            positionX: opts.position.x,
            positionY: opts.position.y,
            positionZ: opts.position.z,
            rotationX: opts.rotation?.x ?? 0,
            rotationY: opts.rotation?.y ?? 0,
            rotationZ: opts.rotation?.z ?? 0,
            rotationW: opts.rotation?.w ?? 1,
          },
        },
      },
    });
    if (opts.expectedRevision === undefined) this.automaticRevisions.add(envelope);
    // Fast path: not congested and no pending queue → immediate send.
    if (this.canSend() && this.latestWinsQueue.size === 0 && this.reliableQueue.length === 0) {
      this.sendEnvelope(envelope);
      return;
    }
    if (this.canSend()) {
      this.flush();
      if (this.canSend() && this.latestWinsQueue.size === 0 && this.reliableQueue.length === 0) {
        this.sendEnvelope(envelope);
        return;
      }
    }
    // Congested → coalesce into latest-wins queue.
    const existed = this.latestWinsQueue.has(key);
    if (existed) {
      this.latestWinsDroppedUpdates++;
      // Move to end to reflect LRU / newest.
      this.latestWinsQueue.delete(key);
    }
    if (!existed && this.latestWinsQueue.size >= LATEST_WINS_MAX_KEYS) {
      const oldestKey = this.latestWinsQueue.keys().next().value as string | undefined;
      if (oldestKey !== undefined) {
        this.latestWinsQueue.delete(oldestKey);
        this.latestWinsCapacityDrops++;
      }
    }
    this.latestWinsQueue.set(key, envelope);
    this.ensureFlushTimer();
  }

  /** Send intent to a registered server rule. Keep the returned request unchanged for retries. */
  sendInput(opts: InputOptions): { request: InputOptions & { commandId: string; expectedRevision: bigint }; result: Promise<InputResult> } {
    this.requireReady();
    const timeoutMs = opts.timeoutMs ?? 10_000;
    if (!opts.rule || !Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) throw new SyncError("INVALID_UPDATE");
    stateBytes(opts.intent, 65536, false);
    const commandId = opts.commandId ?? uuidv7();
    if (this.pendingInputs.has(commandId) || this.pendingInputs.size >= RELIABLE_MAX_QUEUE) throw new SyncError("RESOURCE_LIMIT");
    const expectedRevision = opts.expectedRevision !== undefined ? uint64(opts.expectedRevision, "INVALID_UPDATE")
      : (this.sync?.enabled ? this.sync.entityRevision(opts.entityId) : this.entityRevisions.get(opts.entityId)) ?? 0n;
    const request = { ...opts, intent: structuredClone(opts.intent), commandId, expectedRevision };
    let settle!: (result: InputResult) => void;
    const result = new Promise<InputResult>(resolve => { settle = resolve; });
    const timer = setTimeout(() => this.pendingInputs.get(commandId)?.settle({
      status: "uncertain", commandId, code: "INPUT_TIMEOUT", detail: "input receipt not received",
    }), timeoutMs);
    unrefTimer(timer);
    this.pendingInputs.set(commandId, { settle: value => {
      clearTimeout(timer); this.pendingInputs.delete(commandId); settle(value);
    } });
    try {
      this.sendEntityCommand({ ...request, operation: "input", args: { rule: opts.rule, intent: request.intent } });
    } catch (error) {
      clearTimeout(timer); this.pendingInputs.delete(commandId); throw error;
    }
    return { request, result };
  }

  /** Reliable: EntityCommand (F6 §2 — bounded, never drop). */
  sendEntityCommand(opts: {
    entityId: string;
    operation: string;
    expectedRevision?: number | bigint;
    args?: Record<string, unknown>;
    commandId?: string;
  }): string {
    this.ensureOpen();
    this.requireReady();
    stateBytes(opts.args ?? {}, 65536, false);
    if (!opts.entityId || opts.entityId.length > 256 || opts.operation.length > 256) throw new SyncError("INVALID_UPDATE");
    const commandId = opts.commandId ?? uuidv7();
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/i.test(commandId)) throw new SyncError("INVALID_UPDATE");
    const stored = this.sync?.enabled ? this.sync.entityRevision(opts.entityId) : this.entityRevisions.get(opts.entityId);
    const expectedRevision: bigint =
      opts.expectedRevision !== undefined ? uint64(opts.expectedRevision, "INVALID_UPDATE") : stored ?? 0n;
    const envelope: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: 0,
      messageId: commandId,
      instanceId: this.instanceId,
      payload: {
        case: "entityCommand",
        value: {
          commandId,
          entityId: opts.entityId,
          expectedRevision,
          operation: opts.operation,
          arguments: (opts.args ?? {}) as unknown as Record<string, never>,
        },
      },
    });
    // Validate and own nested arguments now, so caller mutations cannot alter
    // a queued command and invalid input still fails at the call site.
    const owned = clone(EnvelopeSchema, envelope);
    if (opts.expectedRevision === undefined) this.automaticRevisions.add(owned);
    this.enqueueReliable(owned);
    return commandId;
  }

  /**
   * Reliably transfers an entity to another user currently joined to this
   * instance. The server remains authoritative for permission, membership,
   * and revision checks (ADR-027).
   */
  transferEntityOwnership(opts: {
    entityId: string;
    newOwnerId: string;
    expectedRevision?: number | bigint;
  }): string {
    return this.sendEntityCommand({
      entityId: opts.entityId,
      operation: "transfer_ownership",
      expectedRevision: opts.expectedRevision,
      args: { new_owner_id: opts.newOwnerId },
    });
  }

  /** Sends a custom.* event to the current room, including the sender.
   * The server sets data.sender_user_id and data.sender_presence_id.
   * Receiving the echoed event confirms server processing; returning from
   * this method only confirms local queue admission. No chat history is stored.
   */
  sendDomainEvent(opts: { eventType: string; data?: Record<string, unknown> }): string {
    this.ensureOpen();
    this.requireReady();
    stateBytes(opts.data ?? {}, 65536, false);
    if (opts.eventType.length > 256) throw new SyncError("INVALID_UPDATE");
    const eventId = createUuidV7();
    const envelope: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: 0,
      messageId: eventId,
      instanceId: this.instanceId,
      payload: {
        case: "domainEvent",
        value: {
          eventId,
          eventType: opts.eventType,
          instanceRevision: 0n,
          data: (opts.data ?? {}) as unknown as Record<string, never>,
        },
      },
    });
    this.enqueueReliable(clone(EnvelopeSchema, envelope));
    return eventId;
  }

  /** Graceful close maps to server InstanceCommand::Leave (client-sdk.md §3.3). */
  async leave(): Promise<void> {
    if (this.closed) return;
    this._stopSync(new SyncError("CLOSED"), "closed");
    if (this.connectionRef !== null) await this.connectionRef.disconnect();
    else {
      this._close();
      this.ws.close(1000, "leave");
    }
  }

  private emit(event: string, payload: unknown): void {
    if (event === "entityCommand") {
      const receipt = payload as { commandId: string; expectedRevision: bigint; instanceRevision?: bigint };
      if (receipt.instanceRevision !== undefined) this.pendingInputs.get(receipt.commandId)?.settle({
        status: "accepted", commandId: receipt.commandId,
        entityRevision: receipt.expectedRevision, instanceRevision: receipt.instanceRevision,
      });
    } else if (event === "error") {
      const error = payload as { requestMessageId?: string; code: string; message?: string; retryable?: boolean };
      if (error.requestMessageId) this.pendingInputs.get(error.requestMessageId)?.settle({
        status: error.retryable || ["PERSISTENCE_UNAVAILABLE", "REPLAY_WINDOW_EXPIRED"].includes(error.code) ? "uncertain" : "rejected",
        commandId: error.requestMessageId, code: error.code, detail: error.message ?? error.code,
      });
    }
    for (const h of this.handlers.get(event) ?? []) {
      try {
        h(payload);
      } catch (error) {
        if (event !== "error") this.emit("error", listenerFailure(event));
      }
    }
  }

  private resetSnapshotAssembly(): void {
    if (this.snapshotTimeout !== null) clearTimeout(this.snapshotTimeout);
    this.snapshotTimeout = null;
    this.snapshotAssembly = null;
  }

  private applySnapshotRevisions(snapshot: Snapshot): void {
    try {
      if (snapshot.chunkCount < 1 || snapshot.chunkCount > 1024
          || snapshot.chunkIndex >= snapshot.chunkCount) throw new Error("invalid snapshot chunk bounds");
      if (this.snapshotAssembly?.id !== snapshot.snapshotId) {
        this.resetSnapshotAssembly();
        this.snapshotAssembly = { id: snapshot.snapshotId, revision: snapshot.instanceRevision,
          count: snapshot.chunkCount, bytes: 0, chunks: new Map() };
        this.snapshotTimeout = setTimeout(() => {
          this.resetSnapshotAssembly();
          this.emit("error", { code: "INVALID_SNAPSHOT", message: "snapshot chunks did not complete within 10 seconds", retryable: true });
          this.connectionRef?._forceCloseTransport(4000, "incomplete snapshot");
        }, 10_000);
        unrefTimer(this.snapshotTimeout);
      }
      const assembly = this.snapshotAssembly;
      if (assembly.count !== snapshot.chunkCount || assembly.revision !== snapshot.instanceRevision) {
        throw new Error("inconsistent snapshot chunks");
      }
      if (!assembly.chunks.has(snapshot.chunkIndex)) {
        if (assembly.bytes + snapshot.data.length > 16 * 1024 * 1024) throw new Error("snapshot exceeds 16 MiB");
        assembly.chunks.set(snapshot.chunkIndex, snapshot.data.slice());
        assembly.bytes += snapshot.data.length;
      }
      if (assembly.chunks.size !== assembly.count) return;
      const bytes = new Uint8Array(assembly.bytes);
      let offset = 0;
      for (let index = 0; index < assembly.count; index++) {
        const chunk = assembly.chunks.get(index)!;
        bytes.set(chunk, offset); offset += chunk.length;
      }
      const value: unknown = JSON.parse(new TextDecoder("utf-8", { fatal: true }).decode(bytes));
      if (typeof value !== "object" || value === null || !Array.isArray((value as { entities?: unknown }).entities)) {
        throw new Error("snapshot entities are missing");
      }
      const revisions = new Map<string, bigint>();
      for (const entity of (value as { entities: Array<{ entity_id?: unknown; revision?: unknown }> }).entities) {
        if (typeof entity.entity_id !== "string" || typeof entity.revision !== "number"
            || !Number.isSafeInteger(entity.revision) || entity.revision < 0) {
          throw new Error("invalid snapshot entity revision");
        }
        revisions.set(entity.entity_id, BigInt(entity.revision));
      }
      this.entityRevisions = revisions;
      this.connectionRef?._snapshotApplied(assembly.revision);
      this.resetSnapshotAssembly();
      this.emit("snapshotApplied", value);
    } catch (error) {
      this.resetSnapshotAssembly();
      this.emit("error", { code: "INVALID_SNAPSHOT", message: error instanceof Error ? error.message : "invalid snapshot", retryable: true });
      this.connectionRef?._forceCloseTransport(4000, "invalid snapshot");
    }
  }

  // Called by OrbiSyncConnection when a binary frame arrives.
  _dispatch(envelope: Envelope): void {
    if (this.closed) return;
    if (this.initialEvents !== null) {
      this.initialEvents.push(envelope);
      return;
    }
    const p = envelope.payload;
    if (!p) return;
    if (p.case === "domainEvent") this.connectionRef?._revisionApplied(p.value.instanceRevision);
    if (p.case === "stateDelta") this.connectionRef?._revisionApplied(p.value.toRevision);
    const acknowledgedId = p.case === "domainEvent" ? p.value.eventId
      : p.case === "entityCommand" ? p.value.commandId
      : p.case === "error" ? p.value.requestMessageId : undefined;
    if (acknowledgedId && this.unacknowledged.delete(acknowledgedId)) {
      this.reliableQueue = this.reliableQueue.filter(item => item.messageId !== acknowledgedId);
      this.stopFlushTimerIfIdle();
    }
    if ((p.case === "domainEvent" || p.case === "entityCommand") && envelope.messageId) {
      if (this.receivedReliableIds.has(envelope.messageId)) return;
      this.receivedReliableIds.add(envelope.messageId);
      if (this.receivedReliableIds.size > 8192) {
        this.receivedReliableIds.delete(this.receivedReliableIds.values().next().value!);
      }
    }
    if (this.sync?.enabled && ["snapshot", "stateDelta", "entityCommand", "resumeAccepted"].includes(p.case ?? "")) {
      const wasReady = this.sync.status === "ready";
      this.sync.receive(envelope);
      this.connectionRef?._appliedRevision(this.sync.revision);
      if (!wasReady && this.sync.status === "ready") this.connectionRef?._snapshotApplied(this.sync.revision);
      return;
    }
    switch (p.case) {
      case "joinAccepted":
        this.emit("joinAccepted", p.value);
        break;
      case "snapshot": {
        // Decode only after every byte chunk has arrived; individual chunks
        // need not end on a JSON or UTF-8 boundary. Preserve the public chunk
        // events while refreshing revisions before consumers send updates.
        this.applySnapshotRevisions(p.value);
        this.emit("snapshot", p.value);
        break;
      }
      case "stateDelta": {
        // Generated StateDelta has { fromRevision, toRevision, entities: EntityState[] }
        const delta = p.value as unknown as {
          entities: Array<{ entityId: string; revision: number | bigint }>;
        };
        for (const e of delta.entities) {
          const revision = BigInt(e.revision as number);
          const current = this.entityRevisions.get(e.entityId);
          if (current !== undefined && revision <= current) continue;
          this.entityRevisions.set(e.entityId, revision);
          this.emit("entityUpdated", e);
        }
        break;
      }
      case "entityCommand": {
        // Reliable command results carry the entity's resulting revision in
        // expectedRevision. Keep optimistic concurrency in sync for the next
        // update/delete and surface the documented entityUpdated event.
        const command = p.value as EntityCommand;
        if (command.operation === "delete") {
          this.entityRevisions.delete(command.entityId);
        } else if (command.expectedRevision > (this.entityRevisions.get(command.entityId) ?? 0n)) {
          this.entityRevisions.set(command.entityId, command.expectedRevision);
        }
        this.emit("entityCommand", command);
        this.emit("entityUpdated", command);
        break;
      }
      case "error":
        this.emit("error", new RealtimeError(p.value));
        break;
      case "heartbeatAck":
        this.emit("heartbeatAck", p.value);
        break;
      case "resumeAccepted":
        this.emit("resumeAccepted", p.value);
        break;
      case "resyncRequired":
        this.emit("resyncRequired", p.value);
        break;
      default: {
        // DomainEvent and future payloads surface as-is.
        const c = (p as { case?: string; value?: unknown }).case;
        if (c !== undefined) this.emit(c, (p as { value: unknown }).value);
        break;
      }
    }
  }
}

export class OrbiSyncConnection {
  /** Monotonically increasing per-send sequence (client-sdk.md §4.2, RP §4.2). */
  private seq = 0;
  private instance: OrbiSyncInstance | null = null;

  // -- Resume state (W-20, MRIB §2-4) ----------------------------------------
  private resumeToken: string = "";
  private lastAppliedRevision: bigint = 0n;
  private readonly snapshotAppliedListeners = new Set<() => void>();
  private joinedInstanceId: string | null = null;

  // -- Heartbeat state (client-sdk.md §5.4, MRIB §6) -----------------------
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null;
  private heartbeatAckTimeout: ReturnType<typeof setTimeout> | null = null;
  private lastHeartbeatSentMs: bigint | null = null;
  private missedAckCount = 0;
  private lastRttMs: number | null = null;
  private readonly HEARTBEAT_INTERVAL_MS = 20_000; // spec task: every 20s
  private readonly HEARTBEAT_ACK_TIMEOUT_MS = 5_000;
  private readonly HEARTBEAT_MISSED_THRESHOLD = 3; // after N missed acks → reconnect

  // -- Reconnection state (client-sdk.md §5.2, MRIB §2.4) ------------------
  private reconnectAttempt = 0;
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null;
  private closedExplicitly = false;
  private isReconnecting = false;
  private reconnectAbortController: AbortController | null = null;
  private joinWaiterAbortController: AbortController | null = null;
  private readonly RECONNECT_BASE_MS = 1_000;
  private readonly RECONNECT_CAP_MS = 30_000;
  private readonly RECONNECT_MAX_ATTEMPTS = 20; // SDK-03 candidate
  private forceFreshJoin = false;

  _appliedRevision(revision: bigint): void {
    if (this.instance?._syncEnabled()) this.lastAppliedRevision = revision;
  }
  _requestSyncRecovery(error: SyncError): void {
    if (this.closedExplicitly) return;
    if (error.code === "UNSUPPORTED_SERVER" || error.code === "AUTHENTICATION_FAILED") {
      this.failRecovery(error);
      return;
    }
    this.forceFreshJoin = true;
    this.resumeToken = "";
    this.stopHeartbeat();
    this.detachWebSocketHandlers(this.ws, true);
    closeCandidateSocket(this.ws, "synchronization recovery");
    if (!this.isReconnecting) this.scheduleReconnect();
  }

  /** Terminal before callbacks/abort: no old generation may restart recovery. */
  private failRecovery(error: SyncError): void {
    this.closedExplicitly = true;
    this.detachWebSocketHandlers(this.ws, true);
    this.reconnectAbortController?.abort();
    this.reconnectAbortController = null;
    this.cancelJoinWaiter("synchronization recovery exhausted");
    this.isReconnecting = false;
    this.stopHeartbeat();
    if (this.reconnectTimer !== null) clearTimeout(this.reconnectTimer);
    this.reconnectTimer = null;
    this.resumeToken = "";
    this.forceFreshJoin = false;
    this.instance?._stopSync(error, "failed");
    closeCandidateSocket(this.ws, "synchronization unavailable");
  }

  private ws: WebSocket;
  private serverHello: ServerHello;
  private socketGeneration = 0;
  private socketHandlers: SocketHandlers | null = null;
  private readonly connId = Math.random().toString(36).slice(2, 6);
  private connectionPhase: ConnectionPhase = "connected";
  private sessionState: ConnectionState["sessionState"] = "Ready";
  private phaseChangedAtUnixMs = Date.now();
  private lastDisconnect: ConnectionDisconnect | null = null;
  private readonly connectionStateListeners = new Set<ConnectionStateListener>();

  constructor(
    ws: WebSocket,
    serverHello: ServerHello,
    private readonly client: OrbiSyncClient,
  ) {
    this.ws = ws;
    this.serverHello = serverHello;
    this.socketGeneration += 1;
    this.attachWebSocketHandlers(ws);
    // Start periodic heartbeat after ServerHello is accepted.
    // Interval uses ServerHello.heartbeatIntervalMs when provided, else 20s default.
    this.startHeartbeat();
    console.debug(`[orbisync][${this.connId}] new connection serverHello=${serverHello.connectionId}`);
  }

  get hello(): ServerHello {
    return this.serverHello;
  }

  /** Returns the current connection state without exposing resume credentials. */
  getConnectionState(): ConnectionState {
    const lastDisconnect = this.lastDisconnect
      ? Object.freeze({ ...this.lastDisconnect })
      : null;
    return Object.freeze({
      phase: this.connectionPhase,
      sessionState: this.sessionState,
      reconnectAttempt: this.reconnectAttempt,
      lastDisconnect,
      rttMs: this.lastRttMs,
      lastAppliedRevision: this.lastAppliedRevision,
      phaseChangedAtUnixMs: this.phaseChangedAtUnixMs,
    });
  }

  /**
   * Subscribes to typed lifecycle/observability snapshots.
   * Returns an idempotent unsubscribe function.
   */
  onConnectionStateChange(
    listener: ConnectionStateListener,
    options: ConnectionStateSubscriptionOptions = {},
  ): () => void {
    this.connectionStateListeners.add(listener);
    if (options.emitCurrent ?? true) this.deliverConnectionState(listener, this.getConnectionState());
    return () => {
      this.connectionStateListeners.delete(listener);
    };
  }

  private deliverConnectionState(listener: ConnectionStateListener, state: ConnectionState): void {
    try {
      listener(state);
    } catch (error) {
      this.client._reportError(listenerFailure("connectionStateChanged"));
    }
  }

  private emitConnectionState(): void {
    const state = this.getConnectionState();
    for (const listener of [...this.connectionStateListeners]) {
      this.deliverConnectionState(listener, state);
    }
  }

  private setSessionState(state: ConnectionState["sessionState"]): void {
    if (this.sessionState === state) return;
    this.sessionState = state;
    this.emitConnectionState();
  }

  private setConnectionPhase(
    phase: ConnectionPhase,
    disconnect?: ConnectionDisconnect,
  ): void {
    const phaseChanged = phase !== this.connectionPhase;
    const disconnectChanged = disconnect !== undefined && (
      disconnect.code !== this.lastDisconnect?.code ||
      disconnect.reason !== this.lastDisconnect?.reason
    );
    this.connectionPhase = phase;
    if (phaseChanged) this.phaseChangedAtUnixMs = Date.now();
    if (disconnectChanged) this.lastDisconnect = disconnect;
    if (phaseChanged || disconnectChanged) this.emitConnectionState();
  }

  private setLastAppliedRevision(revision: bigint): void {
    if (revision === this.lastAppliedRevision) return;
    this.lastAppliedRevision = revision;
    this.emitConnectionState();
  }

  // -- Sequence management ---------------------------------------------------
  /** Returns next monotonically increasing sequence for this connection (RP §4.2). */
  private nextSequence(): number {
    // Each send direction has independent counter; SDK increments per Envelope.
    // On reconnect a new RealtimeConnectionId resets the counter to 0 (RP §4.2).
    this.seq += 1;
    return this.seq;
  }

  /** Public accessor for OrbiSyncInstance to share the same counter. */
  private nextSequenceForInstance = (): number => this.nextSequence();

  // -- WebSocket handlers ----------------------------------------------------
  private attachWebSocketHandlers(ws: WebSocket): void {
    ws.binaryType = "arraybuffer";
    const generation = this.socketGeneration;
    // ServerHello has already consumed sequence one on this transport.
    let expectedSequence = 2n;
    let invalidTransport = false;
    const failIncoming = (code: string, message: string): void => {
      invalidTransport = true;
      const error = new RealtimeError({ code, message, requestMessageId: "", retryable: true });
      this.client._reportError(error);
      this.instance?._dispatch(create(EnvelopeSchema, { payload: { case: "error", value: {
        code, message, retryable: true,
      } } }));
      this._forceCloseTransport(4000, code);
    };
    const onMessage = (ev: Event): void => {
      if (ws !== this.ws || generation !== this.socketGeneration || invalidTransport) return;
      try {
        const env = decodeEnvelope(wsDataToBytes((ev as MessageEvent).data));
        if (env.sequence < expectedSequence) return;
        if (env.sequence !== expectedSequence) {
          failIncoming("SEQUENCE_GAP", `expected server sequence ${expectedSequence}, received ${env.sequence}`);
          return;
        }
        expectedSequence++;
        this.handleIncomingEnvelope(env);
      } catch (error) {
        this.instance?._syncFailure(error);
        failIncoming("INVALID_MESSAGE", "invalid server protobuf envelope");
      }
    };
    const onClose = (event: Event): void => {
      if (ws !== this.ws || generation !== this.socketGeneration) return;
      const code = (event as CloseEvent).code;
      const reason = (event as CloseEvent).reason;
      console.debug(`[orbisync][${this.connId}] close code=${code} reason=${reason} explicit=${this.closedExplicitly} reconnecting=${this.isReconnecting}`);
      this.stopHeartbeat();
      if (this.heartbeatAckTimeout) {
        clearTimeout(this.heartbeatAckTimeout);
        this.heartbeatAckTimeout = null;
      }
      const disconnect = Object.freeze({
        code: Number.isFinite(code) ? code : null,
        reason: reason || null,
      });
      if (this.closedExplicitly) {
        this.sessionState = "Closed";
        this.setConnectionPhase("closed", disconnect);
        return;
      }
      if (code === 1000) {
        this.setConnectionPhase("offline", disconnect);
        return;
      }
      this.sessionState = "Resuming";
      this.setConnectionPhase("reconnecting", disconnect);
      this.instance?._stopSync(new SyncError(this.closedExplicitly ? "CLOSED" : "DISCONNECTED"), this.closedExplicitly ? "closed" : code === 1000 ? "failed" : "reconnecting");
      if (code === 1000) return;
      if (!this.closedExplicitly && !this.isReconnecting) {
        this.scheduleReconnect();
      }
    };
    const onError = (): void => {
      if (ws !== this.ws || generation !== this.socketGeneration) return;
      // error will be followed by close; scheduleReconnect handles it
    };
    ws.addEventListener("message", onMessage);
    ws.addEventListener("close", onClose);
    ws.addEventListener("error", onError);
    this.socketHandlers = { ws, onMessage, onClose, onError };
  }

  private detachWebSocketHandlers(ws: WebSocket, invalidate = false): void {
    const handlers = this.socketHandlers;
    if (handlers?.ws !== ws) return;
    ws.removeEventListener("message", handlers.onMessage);
    ws.removeEventListener("close", handlers.onClose);
    ws.removeEventListener("error", handlers.onError);
    this.socketHandlers = null;
    if (invalidate && ws === this.ws) this.socketGeneration += 1;
  }

  private swapSocket(ws: WebSocket, serverHello: ServerHello): void {
    const oldWs = this.ws;
    this.cancelJoinWaiter("WebSocket replaced while waiting for JoinAccepted");
    this.detachWebSocketHandlers(oldWs);
    closeCandidateSocket(oldWs, "replaced by reconnect");
    this.ws = ws;
    this.serverHello = serverHello;
    this.socketGeneration += 1;
    this.attachWebSocketHandlers(ws);
    if (this.instance) {
      this.instance._updateWebSocket(ws);
      this.instance._updateNextSequence(this.nextSequenceForInstance);
      this.instance._beginSync(this.socketGeneration, serverHello.enabledFeatures.includes(STATE_SYNC_FEATURE), this.client.getSyncOptions());
    }
  }

  private handleIncomingEnvelope(env: Envelope): void {
    const p = env.payload;
    if (!p) return;

    // Only applied canonical state advances the resume cursor in sync mode.
    switch (p.case) {
      case "joinAccepted": {
        const ja = p.value as JoinAccepted;
        this.instance?._setJoinInfo(ja);
        if (ja.resumeToken) {
          this.resumeToken = ja.resumeToken;
        }
        if (!this.serverHello.enabledFeatures.includes(STATE_SYNC_FEATURE)) {
          this.setLastAppliedRevision(ja.instanceRevision);
        }
        if (ja.presenceId) {
          // keep presence implicit via token; store instance id
        }
        break;
      }
      case "snapshot": break; // Applied only after complete chunk assembly.
      case "domainEvent": {
        if (this.instance?._hasBufferedInitialDelivery()) break;
        if (p.value.instanceRevision > this.lastAppliedRevision) this.setLastAppliedRevision(p.value.instanceRevision);
        break;
      }
      case "stateDelta": {
        if (this.instance?._hasBufferedInitialDelivery()) break;
        const delta = p.value as unknown as { toRevision?: bigint };
        if (!this.serverHello.enabledFeatures.includes(STATE_SYNC_FEATURE)
          && delta.toRevision !== undefined && delta.toRevision > this.lastAppliedRevision) {
          this.setLastAppliedRevision(delta.toRevision);
        }
        break;
      }
      case "resumeAccepted": {
        const ra = p.value as ResumeAccepted;
        // Tokens are single-use and rotate: keep the new one (W-20 Scope 2)
        if (ra.resumeToken) {
          this.resumeToken = ra.resumeToken;
        }
        // The advertised tip is not an acknowledgement: replay/snapshot still
        // has to arrive. Advancing here loses events if replay is interrupted.
        this.reconnectAttempt = 0;
        this.emitConnectionState();
        break;
      }
      case "resyncRequired": {
        // ResyncRequired is handled in doReconnect's await; but if it arrives
        // outside reconnect (e.g., server gap), trigger fresh join fallback
        // asynchronously without blocking dispatch.
        if (this.joinedInstanceId && !this.isReconnecting) {
          this.setConnectionPhase("resyncing");
          void this.doFreshJoin()
            .then(() => this.setConnectionPhase("connected"))
            .catch(() => {
              this.setConnectionPhase("reconnecting", {
                code: null,
                reason: "resync failed",
              });
              this.scheduleReconnect();
            });
        }
        break;
      }
      case "error": {
        this.client._reportError(new RealtimeError(p.value));
        break;
      }
      case "heartbeatAck":
        this.handleHeartbeatAck(p.value);
        break;
    }

    // Also update heartbeat activity for any payload
    if (p.case === "heartbeatAck") {
      // already handled
    }

    this.instance?._dispatch(env);
  }

  // -- Heartbeat -------------------------------------------------------------
  /**
   * Start periodic Heartbeat sends.
   * Sends `Heartbeat { clientTimeUnixMs }` every heartbeatIntervalMs (default 20s).
   * Expects `HeartbeatAck` in return; tracks RTT and missed acks.
   * Per client-sdk.md §5.4 / MRIB §6.
   */
  private startHeartbeat(): void {
    if (this.heartbeatTimer) return;
    const intervalMs =
      this.serverHello.heartbeatIntervalMs > 0n
        ? Number(this.serverHello.heartbeatIntervalMs)
        : this.HEARTBEAT_INTERVAL_MS;
    this.heartbeatTimer = setInterval(() => this.sendHeartbeat(), intervalMs);
    // Unref in Node so timer does not keep process alive in tests.
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (this.heartbeatTimer as any)?.unref?.();
  }

  private stopHeartbeat(): void {
    if (this.heartbeatTimer) {
      clearInterval(this.heartbeatTimer);
      this.heartbeatTimer = null;
    }
    if (this.heartbeatAckTimeout) {
      clearTimeout(this.heartbeatAckTimeout);
      this.heartbeatAckTimeout = null;
    }
  }

  /**
   * Heartbeat is control queue (F6 §3.5) — sent immediately even when congested.
   * BufferedAmount is intentionally ignored here.
   */
  private sendHeartbeat(): void {
    if (this.ws.readyState !== WebSocket.OPEN) return;
    const now = BigInt(Date.now());
    this.lastHeartbeatSentMs = now;
    const envelope: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: this.serverHello.negotiatedMinor,
      messageId: createUuidV7(),
      sequence: BigInt(this.nextSequence()),
      sentAtUnixMs: now,
      instanceId: this.instance?.["instanceId"] ?? this.joinedInstanceId ?? "",
      payload: { case: "heartbeat", value: { clientTimeUnixMs: now } },
    });
    // Control queue: bypass backpressure — always send even if bufferedAmount high.
    this.ws.send(encodeEnvelope(envelope));
    // Arm per-heartbeat timeout (W-20 § Scope 4)
    if (this.heartbeatAckTimeout) clearTimeout(this.heartbeatAckTimeout);
    this.heartbeatAckTimeout = setTimeout(() => this.onHeartbeatMissed(), this.HEARTBEAT_ACK_TIMEOUT_MS);
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (this.heartbeatAckTimeout as any)?.unref?.();
  }

  /**
   * Handle HeartbeatAck — validate echo and compute RTT.
   * Per realtime.proto: HeartbeatAck { client_time_unix_ms, server_time_unix_ms }.
   * Resets missedAckCount; emits `rtt` event in real SDK (client-sdk.md §5.4).
   */
  private handleHeartbeatAck(ack: HeartbeatAck): void {
    // Validate that ack echoes the last sent client time (optional strict check).
    if (this.lastHeartbeatSentMs !== null && ack.clientTimeUnixMs !== this.lastHeartbeatSentMs) {
      // Clock skew or stale ack — log but don't treat as failure.
      console.debug("[orbisync] heartbeat ack mismatch", {
        sent: this.lastHeartbeatSentMs,
        ack: ack.clientTimeUnixMs,
      });
    }
    const rttMs = Number(BigInt(Date.now()) - ack.clientTimeUnixMs);
    this.lastRttMs = rttMs;
    this.missedAckCount = 0;
    this.lastHeartbeatSentMs = ack.clientTimeUnixMs;
    if (this.heartbeatAckTimeout) {
      clearTimeout(this.heartbeatAckTimeout);
      this.heartbeatAckTimeout = null;
    }
    // Emit rtt to instance listeners for observability
    // (instance will forward as "rtt" if anyone listens)
    // We also store for tests.
    this.emitConnectionState();
  }

  /**
   * Called when HeartbeatAck is not received within timeout.
   * Increments missed count; after threshold triggers reconnect.
   */
  private onHeartbeatMissed(): void {
    this.missedAckCount += 1;
    if (this.heartbeatAckTimeout) {
      clearTimeout(this.heartbeatAckTimeout);
      this.heartbeatAckTimeout = null;
    }
    if (this.missedAckCount >= this.HEARTBEAT_MISSED_THRESHOLD) {
      console.warn("[orbisync] heartbeat timeout → reconnecting");
      // Close with non-1000 so close handler triggers reconnect (W-20: kill transport unexpectedly)
      try {
        this.ws.close(4000, "heartbeat timeout");
      } catch {
        // if ws already closed, directly schedule
        this.scheduleReconnect();
      }
      // Fallback if close handler doesn't fire (e.g., already closing)
      if (this.ws.readyState === WebSocket.CLOSED && !this.closedExplicitly) {
        this.scheduleReconnect();
      }
    } else {
      // still arm next? sendHeartbeat will re-arm; but if server stops acking entirely,
      // next heartbeat will also time out and increment.
    }
  }
  // Expose for tests / manual trigger (not part of public API).
  _testOnHeartbeatMissed(): void {
    this.onHeartbeatMissed();
  }

  _getRttMs(): number | null {
    return this.lastRttMs;
  }

  // -- Reconnection (exponential backoff + jitter) ---------------------------
  /**
   * Compute backoff delay with full jitter per MRIB §2.4 / client-sdk.md §5.2:
   *   sleep = random(0, min(cap, base * 2^n))
   * where base=1s, cap=30s, n=attempt (0-based).
   */
  private computeBackoffMs(attempt: number): number {
    const exp = Math.min(this.RECONNECT_CAP_MS, this.RECONNECT_BASE_MS * 2 ** attempt);
    return Math.random() * exp;
  }

  /**
   * Schedules a reconnect attempt with backoff.
   * Wires to real `doReconnect()` (W-20 Scope 3), honouring backoff, cap and max-attempt fields.
   */
  private scheduleReconnect(): void {
    if (this.closedExplicitly) return;
    if (this.isReconnecting) return;
    if (this.reconnectTimer !== null) return;
    if (this.reconnectAttempt >= (this.instance?._syncEnabled() ? 5 : this.RECONNECT_MAX_ATTEMPTS)) {
      console.error(`[orbisync][${this.connId}] reconnect max attempts reached — giving up`);
      this.failRecovery(new SyncError("RECOVERY_EXHAUSTED"));
      return;
    }
    this.setConnectionPhase("reconnecting");
    const delayMs = this.computeBackoffMs(this.reconnectAttempt);
    console.debug(
      `[orbisync][${this.connId}] scheduling reconnect attempt ${this.reconnectAttempt} in ${Math.round(delayMs)}ms`,
    );
    const timer = setTimeout(() => {
      if (this.reconnectTimer !== timer) return;
      this.reconnectTimer = null;
      this.reconnectAttempt += 1;
      this.emitConnectionState();
      void this.doReconnect();
    }, delayMs);
    this.reconnectTimer = timer;
    // eslint-disable-next-line @typescript-eslint/no-explicit-any
    (this.reconnectTimer as any)?.unref?.();
  }

  /** Real reconnect logic — refresh ticket, new WebSocket, ClientHello with resumeToken, ResumeSession (W-20). */
  private async doReconnect(): Promise<void> {
    console.debug(`[orbisync][${this.connId}] doReconnect start attempt=${this.reconnectAttempt} hasToken=${!!this.resumeToken}`);
    if (this.closedExplicitly) return;
    if (this.isReconnecting) return;
    this.isReconnecting = true;
    this.setSessionState("Resuming");
    this.setConnectionPhase("reconnecting");
    const abortController = new AbortController();
    this.reconnectAbortController = abortController;
    this.stopHeartbeat();

    let candidate: WebSocket | null = null;
    try {
      await this.client.ensureValidAccessToken();
      if (this.closedExplicitly || abortController.signal.aborted) return;
      console.debug(`[orbisync][${this.connId}] fetching ticket`);
      const ticket = await this.client.fetchRealtimeTicket();
      if (this.closedExplicitly || abortController.signal.aborted) return;
      console.debug(`[orbisync][${this.connId}] got ticket`);

      // Initial connect and reconnect share this candidate lifecycle. It
      // removes every temporary open/hello listener before returning.
      const wsPath = this.client.getWsPath();
      const wsUrl = this.client.getBaseUrl().replace(/^http/, "ws") + wsPath;
      this.seq = 0;
      const candidateResult = await openCandidateSocket({
        wsUrl,
        subprotocol: this.client.getSubprotocol(),
        ticket,
        resumeToken: this.resumeToken ?? "",
        helloSequence: this.nextSequence(),
        clientName: this.client.getClientName(),
        clientVersion: this.client.getClientVersion(),
        clientType: this.client.getClientType(),
        signal: abortController.signal,
      });
      candidate = candidateResult.ws;
      if (this.closedExplicitly || abortController.signal.aborted) { closeCandidateSocket(candidate); return; }
      console.debug(`[orbisync][${this.connId}] got ServerHello ${candidateResult.serverHello.connectionId}`);

      // Ordering is intentional: detach old -> close old -> publish/attach new.
      if (this.instance?._syncEnabled() && !candidateResult.serverHello.enabledFeatures.includes(STATE_SYNC_FEATURE)) {
        throw new SyncError("UNSUPPORTED_SERVER");
      }
      this.swapSocket(candidate, candidateResult.serverHello);
      const snapshotReady = this.instance ? this.awaitSnapshotApplied(abortController.signal) : Promise.resolve();
      // The failure path aborts this waiter; keep its rejection observed even
      // when an earlier handshake/resume step fails first.
      void snapshotReady.catch(() => {});

      // Publish before waiting so replayed state frames are dispatched while
      // the temporary result waiter observes the resume response.
      if (!this.forceFreshJoin && this.resumeToken && this.joinedInstanceId) {
        const resumeSeq = this.nextSequence();
        const resumeEnv: Envelope = create(EnvelopeSchema, {
          protocolMajor: PROTOCOL_MAJOR,
          protocolMinor: candidateResult.serverHello.negotiatedMinor,
          messageId: createUuidV7(),
          sequence: BigInt(resumeSeq),
          sentAtUnixMs: BigInt(Date.now()),
          instanceId: this.joinedInstanceId,
          payload: {
            case: "resumeSession",
            value: {
              resumeToken: this.resumeToken,
              lastAppliedRevision: this.lastAppliedRevision,
              receivedMessageIds: this.instance?._receivedMessageIds() ?? [],
            },
          },
        });
        const resumeResult = this.awaitResumeResult(this.ws, 8000, abortController.signal);
        this.ws.send(encodeEnvelope(resumeEnv));
        const result = await resumeResult;
        // Metadata was already applied in wire order. Reapplying it here
        // could move revision backwards past a coalesced replayed delta.
        if (result.case !== "resumeAccepted") {
          this.instance?._freshSession();
          this.setConnectionPhase("resyncing");
          await this.doFreshJoin(abortController.signal);
        }
      } else if (this.joinedInstanceId) {
        this.instance?._freshSession();
        this.setConnectionPhase("resyncing");
        await this.doFreshJoin(abortController.signal);
      }

      if (this.instance?._syncEnabled()) await this.instance.ready({ signal: abortController.signal, timeoutMs: this.client.getSyncOptions()?.timeoutMs ?? 10_000 });
      if (this.closedExplicitly || abortController.signal.aborted) return;
      this.forceFreshJoin = false;
      await snapshotReady;
      this.reconnectAttempt = 0;
      this.isReconnecting = false;
      this.reconnectAbortController = null;
      this.missedAckCount = 0;
      this.startHeartbeat();
      this.setSessionState(this.instance ? "Active" : "Ready");
      this.setConnectionPhase("connected");
    } catch (err) {
      abortController.abort();
      console.debug("[orbisync] doReconnect failed, will retry");
      if (candidate !== null) {
        this.detachWebSocketHandlers(candidate, true);
        closeCandidateSocket(candidate);
      }
      this.isReconnecting = false;
      this.reconnectAbortController = null;
      if (err instanceof RefreshFailedError) {
        this.stopHeartbeat();
        this.setConnectionPhase("offline");
        this.instance?._dispatch(create(EnvelopeSchema, { payload: { case: "error", value: {
          code: err.code, message: err.message, retryable: false,
        } } }));
        return;
      }
      if (!this.closedExplicitly && this.reconnectAttempt < this.RECONNECT_MAX_ATTEMPTS) {
        this.scheduleReconnect();
      } else {
        this.stopHeartbeat();
        if (!this.closedExplicitly) this.setConnectionPhase("offline");
      }
      if (err instanceof SyncError && ["AUTHENTICATION_FAILED", "UNSUPPORTED_SERVER"].includes(err.code)) {
        this._requestSyncRecovery(err);
      } else if (!this.closedExplicitly) {
        if (err instanceof SyncError && err.code === "SYNC_TIMEOUT") {
          this.forceFreshJoin = true;
          this.resumeToken = "";
        }
        this.instance?._stopSync(new SyncError("DISCONNECTED"));
        this.scheduleReconnect();
      } else this.stopHeartbeat();
    }
  }
  private awaitServerHello(ws: WebSocket, timeoutMs: number): Promise<ServerHello> {
    return waitForServerHello(ws, timeoutMs);
  }

  private awaitResumeResult(
    ws: WebSocket,
    timeoutMs: number,
    signal?: AbortSignal,
  ): Promise<{ case: "resumeAccepted" | "resyncRequired" | "error"; value: unknown }> {
    return waitForResumeResult(ws, timeoutMs, signal);
  }

  private async doFreshJoin(signal?: AbortSignal): Promise<void> {
    if (!this.joinedInstanceId) return;
    this.setSessionState("Joining");
    // Send JoinInstance and await JoinAccepted + Snapshot
    const seq = this.nextSequence();
    const env: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: this.serverHello.negotiatedMinor,
      messageId: createUuidV7(),
      sequence: BigInt(seq),
      sentAtUnixMs: BigInt(Date.now()),
      instanceId: "",
      payload: { case: "joinInstance", value: { worldInstanceId: this.joinedInstanceId } },
    });
    const waiting = this.awaitJoinAccepted(5000, signal);
    try { this.ws.send(encodeEnvelope(env)); }
    catch (error) { this.cancelJoinWaiter("join send failed"); await waiting.catch(() => {}); throw error; }
    const accepted = await waiting;
    // awaitJoinAccepted already updates resumeToken via handleIncoming, but also return value
    if (accepted.resumeToken) {
      this.resumeToken = accepted.resumeToken;
    }
    if (!this.instance?._syncEnabled()) this.lastAppliedRevision = accepted.instanceRevision;
    this.reconnectAttempt = 0;
  }

  private cancelJoinWaiter(reason: string): void {
    const controller = this.joinWaiterAbortController;
    if (!controller) return;
    this.joinWaiterAbortController = null;
    controller.abort(reason);
  }

  private awaitJoinAccepted(
    timeoutMs: number,
    signal?: AbortSignal,
    onAccepted?: (accepted: JoinAccepted) => void,
  ): Promise<JoinAccepted> {
    return new Promise<JoinAccepted>((resolve, reject) => {
      const ws = this.ws;
      const generation = this.socketGeneration;
      const waiterController = new AbortController();
      const previousController = this.joinWaiterAbortController;
      if (previousController) previousController.abort("JoinAccepted wait superseded");
      this.joinWaiterAbortController = waiterController;
      let timer: ReturnType<typeof setTimeout> | null = null;
      let settled = false;
      const cleanup = (): void => {
        if (timer !== null) clearTimeout(timer);
        ws.removeEventListener("message", onMessage);
        ws.removeEventListener("error", onError);
        ws.removeEventListener("close", onClose);
        waiterController.signal.removeEventListener("abort", onAbort);
        signal?.removeEventListener("abort", onAbort);
        if (this.joinWaiterAbortController === waiterController) {
          this.joinWaiterAbortController = null;
        }
      };
      const succeed = (value: JoinAccepted): void => {
        if (settled) return;
        settled = true;
        cleanup();
        resolve(value);
      };
      const fail = (message: string | Error): void => {
        if (settled) return;
        settled = true;
        cleanup();
        reject(message instanceof Error ? message : new Error(message));
      };
      const onMessage = (ev: MessageEvent): void => {
        if (ws !== this.ws || generation !== this.socketGeneration) {
          fail("WebSocket replaced while waiting for JoinAccepted");
          return;
        }
        try {
          const env = decodeEnvelope(wsDataToBytes(ev.data));
          if (env.payload?.case === "joinAccepted") {
            onAccepted?.(env.payload.value);
            succeed(env.payload.value);
          } else if (env.payload?.case === "error") {
            fail(new RealtimeError(env.payload.value));
          }
        } catch {
          // ignore
        }
      };
      const onError = (): void => fail("WebSocket error while waiting for JoinAccepted");
      const onClose = (): void => fail("WebSocket closed while waiting for JoinAccepted");
      const onAbort = (): void => {
        const reason = waiterController.signal.reason;
        fail(typeof reason === "string" ? reason : "WebSocket lifecycle aborted");
      };
      if (signal?.aborted || waiterController.signal.aborted) {
        onAbort();
        return;
      }
      timer = setTimeout(() => fail("JoinAccepted timeout"), timeoutMs);
      try {
        ws.addEventListener("message", onMessage);
        ws.addEventListener("error", onError);
        ws.addEventListener("close", onClose);
        waiterController.signal.addEventListener("abort", onAbort);
        signal?.addEventListener("abort", onAbort);
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        (timer as any)?.unref?.();
      } catch {
        fail("JoinAccepted listener setup failed");
      }
    });
  }

  /** Reset backoff after a successful connection (call after ServerHello). */
  private resetReconnectState(): void {
    this.reconnectAttempt = 0;
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
  }

  // Exposed for tests / observability
  _getReconnectAttempt(): number {
    return this.reconnectAttempt;
  }
  _computeBackoffMs(attempt: number): number {
    return this.computeBackoffMs(attempt);
  }
  _getResumeToken(): string {
    return this.resumeToken;
  }
  _getLastAppliedRevision(): bigint {
    return this.lastAppliedRevision;
  }

  /** Internal acknowledgement after a complete, valid snapshot is assembled. */
  _snapshotApplied(revision: bigint): void {
    // Initial delivery can be buffered while newer deltas have already arrived.
    // Applying that snapshot must not move the reconnect cursor backwards.
    if (revision > this.lastAppliedRevision) this.setLastAppliedRevision(revision);
    if (this.connectionPhase === "connected") this.setSessionState("Active");
    for (const listener of this.snapshotAppliedListeners) listener();
  }

  _revisionApplied(revision: bigint): void {
    if (revision > this.lastAppliedRevision) this.setLastAppliedRevision(revision);
  }

  private awaitSnapshotApplied(signal: AbortSignal): Promise<void> {
    return new Promise((resolve, reject) => {
      const cleanup = () => {
        clearTimeout(timer);
        this.snapshotAppliedListeners.delete(complete);
        signal.removeEventListener("abort", abort);
      };
      const complete = () => { cleanup(); resolve(); };
      const abort = () => { cleanup(); reject(new Error("snapshot wait aborted")); };
      const timer = setTimeout(() => { cleanup(); reject(new Error("snapshot application timed out")); }, 10_000);
      unrefTimer(timer);
      this.snapshotAppliedListeners.add(complete);
      signal.addEventListener("abort", abort, { once: true });
      if (signal.aborted) abort();
    });
  }
  _getWs(): WebSocket {
    return this.ws;
  }
  // Test helper to force reconnect for mutation test bypass
  _setResumeToken(token: string): void {
    this.resumeToken = token;
  }

  /** Expose control-queue length and heartbeat metrics (F6 §3.6). */
  getQueueMetrics(): {
    latestWins: { queueLength: number; droppedUpdates: number; capacityDrops: number };
    reliable: { queueLength: number; inFlight: number; saturationErrors: number };
    control: { queueLength: number };
  } {
    // Control queue (heartbeat) is always 0 because heartbeats bypass backpressure.
    // If we have an instance, delegate its queues; otherwise report empty.
    if (this.instance) {
      const m = this.instance.getQueueMetrics();
      return { latestWins: m.latestWins, reliable: m.reliable, control: { queueLength: 0 } };
    }
    return {
      latestWins: { queueLength: 0, droppedUpdates: 0, capacityDrops: 0 },
      reliable: { queueLength: 0, inFlight: 0, saturationErrors: 0 },
      control: { queueLength: 0 },
    };
  }

  /** For OrbiSyncInstance.leave() to mark explicit close without exposing private. */
  _markExplicitClose(): void {
    this.closedExplicitly = true;
    this.sessionState = "Closed";
    this.setConnectionPhase("closed", { code: 1000, reason: "leave" });
  }

  /** Keep application sends queued until a resumed/fresh joined session is ready. */
  _canSendInstanceMessages(): boolean {
    return this.connectionPhase === "connected" && this.sessionState === "Active" && !this.closedExplicitly;
  }

  /** JoinInstance → JoinAccepted + Snapshot (client-sdk.md §3.3). */
  async join(worldInstanceId: string): Promise<OrbiSyncInstance> {
    if (this.closedExplicitly || this.connectionPhase === "closed") throw new Error("connection is closed");
    if (this.connectionPhase !== "connected") throw new Error("connection is not ready to join");
    if (this.instance !== null) throw new Error("connection already joined an instance");
    if (this.joinWaiterAbortController !== null) throw new Error("join already in progress");
    const previousInstanceId = this.joinedInstanceId;
    const previousInstance = this.instance;
    this.setSessionState("Joining");
    this.joinedInstanceId = worldInstanceId;
    const envelope: Envelope = create(EnvelopeSchema, {
      protocolMajor: PROTOCOL_MAJOR,
      protocolMinor: this.serverHello.negotiatedMinor,
      messageId: createUuidV7(),
      sequence: BigInt(this.nextSequence()),
      sentAtUnixMs: BigInt(Date.now()),
      instanceId: "",
      payload: { case: "joinInstance", value: { worldInstanceId } },
    });
    const inst = new OrbiSyncInstance(this.ws, worldInstanceId, this.nextSequenceForInstance);
    inst._setConnection(this);
    inst._beginSync(this.socketGeneration, this.serverHello.enabledFeatures.includes(STATE_SYNC_FEATURE), this.client.getSyncOptions());
    // Install the waiter before sending. Attach the instance synchronously when
    // JoinAccepted arrives, before a coalesced Snapshot/StateDelta can follow.
    const joining = this.awaitJoinAccepted(5000, undefined, (accepted) => {
      inst._beginInitialDelivery(accepted);
      this.instance = inst;
    });
    try {
      this.ws.send(encodeEnvelope(envelope));
      await joining;
      // Resume metadata is already updated in wire order by handleIncomingEnvelope.
      inst._finishInitialDelivery();
      return inst;
    } catch (error) {
      this.cancelJoinWaiter("JoinInstance send failed");
      await joining.catch(() => {});
      inst._cancelInitialDelivery();
      inst._stopSync(new SyncError("CLOSED"), "closed");
      if (this.instance === inst) this.instance = previousInstance;
      this.joinedInstanceId = previousInstanceId;
      if (!this.closedExplicitly) this.setSessionState("Ready");
      throw error;
    }
  }

  /**
   * Requests a fresh transport and exercises the normal automatic resume path.
   * Joined instance state and queued reliable messages are preserved.
   */
  requestReconnect(): void {
    if (this.closedExplicitly || this.connectionPhase === "closed") {
      throw new Error("connection is closed");
    }
    if (this.connectionPhase === "offline") {
      this.reconnectAttempt = 0;
      void this.doReconnect();
      return;
    }
    if (this.connectionPhase !== "connected") {
      throw new Error(`connection is already ${this.connectionPhase}`);
    }
    this.ws.close(4000, "client requested reconnect");
  }

  async disconnect(): Promise<void> {
    this.closedExplicitly = true;
    this.sessionState = "Closed";
    this.setConnectionPhase("closed", { code: 1000, reason: "disconnect" });
    this.isReconnecting = false;
    this.instance?._stopSync(new SyncError("CLOSED"), "closed");
    this.reconnectAbortController?.abort();
    this.reconnectAbortController = null;
    this.cancelJoinWaiter("WebSocket lifecycle aborted");
    this.stopHeartbeat();
    if (this.reconnectTimer) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.heartbeatAckTimeout) {
      clearTimeout(this.heartbeatAckTimeout);
      this.heartbeatAckTimeout = null;
    }
    // Clear any pending send-queue timer on the instance.
    this.instance?._close();
    const ws = this.ws;
    this.detachWebSocketHandlers(ws, true);
    try {
      ws.close(1000, "disconnect");
    } catch {
      closeCandidateSocket(ws, "disconnect");
    }
    this.connectionStateListeners.clear();
  }

  /** Force-close underlying socket with non-1000 code to simulate unexpected transport loss (test helper). */
  _forceCloseTransport(code = 1011, reason = "transport lost"): void {
    try {
      this.ws.close(code, reason);
    } catch {
      // fallback: terminate-like
      if ((this.ws as unknown as { terminate?: () => void }).terminate) {
        (this.ws as unknown as { terminate: () => void }).terminate();
      }
    }
  }
}

export class RefreshFailedError extends Error {
  readonly code = "RefreshFailed";
  readonly retryable = false;
  constructor(_cause: unknown) {
    super("Authentication refresh failed; log in again");
    this.name = "RefreshFailedError";
  }
}

export type AuthState = "Unauthenticated" | "Authenticating" | "Authenticated";

export class OrbiSyncClient {
  private authState: AuthState = "Unauthenticated";
  private readonly authListeners = new Map<string, Set<(value: unknown) => void>>();

  getAuthState(): AuthState { return this.authState; }

  on(event: "error" | "authStateChanged" | "connectionStateChanged", handler: (value: unknown) => void): void {
    const handlers = this.authListeners.get(event) ?? new Set();
    handlers.add(handler);
    this.authListeners.set(event, handlers);
  }

  off(event: "error" | "authStateChanged" | "connectionStateChanged", handler: (value: unknown) => void): void {
    this.authListeners.get(event)?.delete(handler);
  }

  private emitAuth(event: string, value: unknown): void {
    for (const handler of this.authListeners.get(event) ?? []) {
      try { handler(value); } catch {
        if (event !== "error") this.emitAuth("error", listenerFailure(event));
      }
    }
  }

  /** Shared error channel for connections created by this client. */
  _reportError(error: RealtimeError): void { this.emitAuth("error", error); }

  private setAuthState(state: AuthState): void {
    if (this.authState === state) return;
    this.authState = state;
    this.emitAuth("authStateChanged", state);
    this.emitAuth("connectionStateChanged", { authState: state });
  }
  private accessToken: string | null = null;
  private refreshToken: string | null = null;
  private accessTokenExpiresAt: number | null = null;
  private accessTokenRefreshAt: number | null = null;
  private authGeneration = 0;
  private connectInFlight: Promise<OrbiSyncConnection> | null = null;
  private refreshInFlight: Promise<void> | null = null;
  private readonly opts: ClientOptions;

  constructor(opts: ClientOptions) {
    this.opts = { ...opts, baseUrl: opts.baseUrl.replace(/\/+$/, "") };
  }

  getSyncOptions(): SyncOptions | undefined { return this.opts.sync; }

  getBaseUrl(): string {
    return this.opts.baseUrl;
  }
  getWsPath(): string {
    return this.opts.wsPath ?? "/ws";
  }
  getSubprotocol(): string {
    return this.opts.subprotocol ?? WEBSOCKET_SUBPROTOCOL;
  }
  getClientName(): string {
    return this.opts.clientName ?? "@orbisync/client";
  }
  getClientVersion(): string {
    return this.opts.clientVersion ?? "0.1.0";
  }
  getClientType(): string {
    return this.opts.clientType ?? "desktop";
  }

  /** For tests: inject tokens directly (bypasses login). */
  _setTokens(accessToken: string, refreshToken?: string, expiresInSec?: number): void {
    this.authGeneration++;
    this.refreshInFlight = null;
    this.connectInFlight = null;
    this.accessToken = accessToken;
    this.refreshToken = refreshToken ?? null;
    this.setTokenLifetime(expiresInSec);
    this.setAuthState("Authenticated");
  }

  private setTokenLifetime(expiresInSec?: number): void {
    const now = Date.now();
    const lifetime = (expiresInSec ?? 15 * 60) * 1000;
    this.accessTokenExpiresAt = now + lifetime;
    this.accessTokenRefreshAt = now + lifetime * 0.8;
  }
  _getAccessToken(): string | null {
    return this.accessToken;
  }

  async ensureValidAccessToken(): Promise<void> {
    // If we have an expiry, proactively refresh at 80% (client-sdk.md §2.2)
    if (this.accessToken && this.accessTokenRefreshAt !== null && this.refreshToken
        && Date.now() >= this.accessTokenRefreshAt) {
      await this.refreshAccessToken();
    }
    // If no access token, caller will fail; don't refresh blindly
  }

  async refreshAccessToken(): Promise<void> {
    if (this.refreshInFlight !== null) return this.refreshInFlight;
    const operation = this.performRefreshAccessToken();
    this.refreshInFlight = operation;
    try {
      await operation;
    } finally {
      if (this.refreshInFlight === operation) this.refreshInFlight = null;
    }
  }

  private async performRefreshAccessToken(): Promise<void> {
    const generation = this.authGeneration;
    try {
      if (!this.refreshToken) throw new Error("no refresh token available");
      const res = await fetchTransport(`${this.opts.baseUrl}/v1/auth/refresh`, {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ refresh_token: this.refreshToken }),
      });
      if (!res.ok) throw new Error(`refresh failed: ${res.status}`);
      const pair = (await credentialJson(res)) as { access_token: string; refresh_token?: string; expires_in?: number };
      if (generation !== this.authGeneration) throw new Error("authentication operation superseded");
      this.accessToken = pair.access_token;
      if (pair.refresh_token) this.refreshToken = pair.refresh_token;
      this.setTokenLifetime(pair.expires_in);
      this.setAuthState("Authenticated");
    } catch (error) {
      if (generation !== this.authGeneration) throw new Error("authentication operation superseded");
      this.accessToken = null;
      this.refreshToken = null;
      this.accessTokenExpiresAt = null;
      this.accessTokenRefreshAt = null;
      this.setAuthState("Unauthenticated");
      const failure = new RefreshFailedError(error);
      this.emitAuth("error", failure);
      throw failure;
    }
  }

  /**
   * Performs an authenticated same-origin REST request without exposing tokens.
   * A 401 response refreshes the access token and retries once when possible.
   */
  async fetch(path: string, init: RequestInit = {}): Promise<Response> {
    if (!path.startsWith("/") || path.startsWith("//")) {
      throw new Error("SDK fetch path must be an absolute same-origin path beginning with /");
    }
    if (!this.accessToken) throw new Error("not authenticated: call auth.login first");

    await this.ensureValidAccessToken();
    const perform = (): Promise<Response> => {
      const headers = new Headers(init.headers);
      headers.set("Authorization", `Bearer ${this.accessToken}`);
      return fetchTransport(`${this.opts.baseUrl}${path}`, { ...init, headers });
    };

    let response = await perform();
    if (response.status === 401 && this.refreshToken) {
      await this.refreshAccessToken();
      response = await perform();
    }
    return response;
  }

  async fetchRealtimeTicket(): Promise<string> {
    if (!this.accessToken) throw new Error("not authenticated: call auth.login first");
    // Attempt ticket fetch; on 401 try refresh once (client-sdk.md §2.2)
    let res = await fetchTransport(`${this.opts.baseUrl}/v1/realtime/tickets`, {
      method: "POST",
      headers: { Authorization: `Bearer ${this.accessToken}` },
    });
    if (res.status === 401 && this.refreshToken) {
      await this.refreshAccessToken();
      res = await fetchTransport(`${this.opts.baseUrl}/v1/realtime/tickets`, {
        method: "POST",
        headers: { Authorization: `Bearer ${this.accessToken}` },
      });
    }
    if (res.status === 429) {
      throw new RealtimeTicketRateLimitedError(`ticket rate limited: ${res.status}`);
    }
    if (res.status === 401 || res.status === 403) throw new SyncError("AUTHENTICATION_FAILED");
    if (!res.ok) throw new Error(`ticket failed: ${res.status}`);
    const json = (await credentialJson(res)) as unknown;
    const parsed: RealtimeTicketResponse = parseRealtimeTicketResponse(json);
    return parsed.realtime_ticket;
  }

  /**
   * Stores an issued token pair.
   *
   * Every authentication method returns the same body, so they all land here
   * and `fetchRealtimeTicket`, `connect` and `join` work identically
   * afterwards — the SDK has no per-method connection path.
   */
  private applyTokenPair(pair: TokenPair): void {
    this.accessToken = pair.access_token;
    if (pair.refresh_token) this.refreshToken = pair.refresh_token;
    this.accessTokenExpiresAt =
      Date.now() + (pair.expires_in !== undefined ? pair.expires_in * 1000 : 15 * 60 * 1000);
  }

  /**
   * Posts to an authentication endpoint and stores the resulting tokens.
   *
   * A disabled method answers 403 `AUTH_METHOD_DISABLED`, which surfaces as
   * {@link AuthMethodDisabledError} so a caller can fall back to another
   * method instead of treating it as a credential failure.
   */
  private async authenticateVia(path: string, body?: unknown): Promise<void> {
    const res = await fetch(`${this.opts.baseUrl}${path}`, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify(body ?? {}),
    });
    if (res.status === 403) {
      const detail = (await res.json().catch(() => null)) as ErrorEnvelope | null;
      if (detail?.error?.code === "AUTH_METHOD_DISABLED") {
        throw new AuthMethodDisabledError(`${path} is not enabled on this server`);
      }
    }
    if (!res.ok) throw new Error(`${path} failed: ${res.status}`);
    this.applyTokenPair((await res.json()) as TokenPair);
  }

  readonly auth = {
    /**
     * Lists the authentication methods this server accepts.
     *
     * Reachable before authenticating, because a client has to know which call
     * to make first. Returns identifiers only.
     */
    methods: async (): Promise<AuthMethod[]> => {
      const res = await fetch(`${this.opts.baseUrl}/v1/auth/methods`, { method: "GET" });
      if (!res.ok) throw new Error(`auth methods failed: ${res.status}`);
      const body = (await res.json()) as { methods?: unknown };
      if (!Array.isArray(body.methods)) throw new Error("auth methods response is malformed");
      return body.methods.filter((m): m is AuthMethod => typeof m === "string" && AUTH_METHODS.includes(m as AuthMethod));
    },

    login: async (args: { loginId: string; password: string }): Promise<void> => {
      const generation = ++this.authGeneration;
      this.refreshInFlight = null;
      this.connectInFlight = null;
      this.setAuthState("Authenticating");
      try {
        this.accessToken = null;
        this.refreshToken = null;
        this.accessTokenExpiresAt = null;
        this.accessTokenRefreshAt = null;
        const res = await fetchTransport(`${this.opts.baseUrl}/v1/auth/login`, {
          method: "POST",
          headers: { "Content-Type": "application/json" },
          body: JSON.stringify({ login_id: args.loginId, password: args.password }),
        });
        if (!res.ok) throw new Error(`login failed: ${res.status}`);
        const pair = (await credentialJson(res)) as { access_token: string; refresh_token?: string; expires_in?: number };
        if (generation !== this.authGeneration) throw new Error("authentication operation superseded");
        this.accessToken = pair.access_token;
        if (pair.refresh_token) this.refreshToken = pair.refresh_token;
        this.setTokenLifetime(pair.expires_in);
        this.setAuthState("Authenticated");
      } catch (error) {
        if (generation === this.authGeneration) this.setAuthState("Unauthenticated");
        throw error;
      }
    },

    /**
     * Joins anonymously. The server decides the identifier, the display name
     * and the roles; nothing is sent.
     */
    guest: async (): Promise<void> => {
      await this.authenticateVia("/v1/auth/guest");
    },

    /**
     * Joins with a display name only.
     *
     * The name is a label, not a credential: two visitors who choose the same
     * one still receive distinct identities.
     */
    nameOnly: async (args: { displayName: string }): Promise<void> => {
      await this.authenticateVia("/v1/auth/name", { display_name: args.displayName });
    },

    /**
     * Authenticates with a token from the server's configured issuer.
     */
    external: async (args: { token: string }): Promise<void> => {
      await this.authenticateVia("/v1/auth/external", { token: args.token });
    },
  };

  /**
   * Establishes a realtime connection:
   * 1) POST /v1/realtime/tickets (Bearer token)
   * 2) WebSocket with `WEBSOCKET_SUBPROTOCOL`
   * 3) Send ClientHello, await ServerHello → Ready (client-sdk.md §3.1)
   *
   * Sequence for ClientHello starts at 1 and then increments monotonically
   * per send on the resulting OrbiSyncConnection (client-sdk.md §4.2).
   */
  async connect(): Promise<OrbiSyncConnection> {
    if (!this.accessToken) throw new Error("not authenticated: call auth.login first");
    if (this.connectInFlight !== null) return this.connectInFlight;
    const operation = this.connectInternal();
    this.connectInFlight = operation;
    try {
      return await operation;
    } finally {
      if (this.connectInFlight === operation) this.connectInFlight = null;
    }
  }

  private async connectInternal(): Promise<OrbiSyncConnection> {
    const generation = this.authGeneration;
    await this.ensureValidAccessToken();
    const ticket = await this.fetchRealtimeTicket();
    if (generation !== this.authGeneration) throw new Error("authentication operation superseded");
    const wsPath = this.opts.wsPath ?? "/ws";
    const wsUrl = this.opts.baseUrl.replace(/^http/, "ws") + wsPath;
    const seq = 1;
    const result = await openCandidateSocket({
      wsUrl,
      subprotocol: this.opts.subprotocol ?? WEBSOCKET_SUBPROTOCOL,
      ticket,
      resumeToken: "",
      helloSequence: seq,
      clientName: this.opts.clientName ?? "@orbisync/client",
      clientVersion: this.opts.clientVersion ?? "0.1.0",
      clientType: this.opts.clientType ?? "desktop",
    });

    // Hand off sequence counter; connection continues from current seq.
    if (generation !== this.authGeneration) {
      closeCandidateSocket(result.ws, "authentication operation superseded");
      throw new Error("authentication operation superseded");
    }
    const conn = new OrbiSyncConnection(result.ws, result.serverHello, this);
    (conn as unknown as { seq: number }).seq = seq;
    return conn;
  }
  /**
   * Reconnection helper with exponential backoff + jitter (client-sdk.md §5.2).
   * Stub: wraps `connect()` with retry. Useful for callers that want auto-retry
   * at the Client level (Connection-level backoff is inside OrbiSyncConnection).
   *
   * Backoff: base=1s, cap=30s, full jitter — per MRIB §2.4.
   */
  async connectWithRetry(maxAttempts = 5): Promise<OrbiSyncConnection> {
    const baseMs = 1_000;
    const capMs = 30_000;
    for (let attempt = 0; attempt < maxAttempts; attempt++) {
      try {
        return await this.connect();
      } catch (err) {
        if (attempt === maxAttempts - 1) throw err;
        const exp = Math.min(capMs, baseMs * 2 ** attempt);
        const delayMs = Math.random() * exp;
        await new Promise((r) => setTimeout(r, delayMs));
      }
    }
    throw new Error("connectWithRetry: unreachable");
  }
}

// ---------------------------------------------------------------------------
// Example usage (client-sdk.md §10) — not executed, for docs / E2E sketch:
// ---------------------------------------------------------------------------

async function _example(): Promise<void> {
  const client = new OrbiSyncClient({ baseUrl: "https://meta.example.org" });
  await client.auth.login({ loginId: "user001", password: "temporary-password" });
  const connection = await client.connect();
  const instance = await connection.join("0192d43d-a18a-7fed-8123-0123456789ab");
  instance.on("snapshot", (s) => console.log("snapshot", s));
  instance.on("entityUpdated", (u) => console.log("entityUpdated", u));
  instance.sendTransform({
    entityId: "my-entity",
    position: { x: 1, y: 0, z: 3 },
    rotation: { x: 0, y: 0, z: 0, w: 1 },
  });
  await instance.leave();
}
void _example;
