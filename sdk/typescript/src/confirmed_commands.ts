import type { EntityCommand } from "./generated/orbisync/v1/realtime_pb.js";
import { stateBytes, uint64, SyncError } from "./sync_state.js";
import { uuidv7 } from "./uuidv7.js";

export type EntityCommandOptions = {
  entityId: string; operation: string; expectedRevision?: number | bigint;
  args?: Record<string, unknown>; commandId?: string;
};
export type ConfirmedCommandRequest = Readonly<{
  entityId: string; operation: string; expectedRevision: bigint;
  args: Readonly<Record<string, unknown>>; commandId: string;
}>;
export type CommandResult =
  | Readonly<{ status: "committed"; commandId: string; entityRevision: bigint; instanceRevision: bigint }>
  | Readonly<{ status: "rejected" | "unknown" | "ended"; commandId: string; code: string; detail: string }>;
export type CommandState = Readonly<{
  commandId: string; entityId: string; operation: string; attempts: number;
  status: "pending" | CommandResult["status"]; result?: CommandResult;
}>;
export const CONFIRMED_COMMAND_MAX_PENDING = 256;
export const CONFIRMED_COMMAND_MAX_BYTES = 4 * 1024 * 1024;

type Transport = {
  readonly syncStatus: string;
  entityRevision(id: string): bigint | undefined;
  sendEntityCommand(request: EntityCommandOptions): string;
  _forgetQueuedCommand(id: string): void;
};
type Entry = {
  handle: TrackedEntityCommand; request: ConfirmedCommandRequest; bytes: number;
  attempts: number; unknown: boolean; result?: CommandResult;
  waiter?: { resolve: (result: CommandResult) => void; timer: ReturnType<typeof setTimeout> };
};

function freezeJson<T>(value: T): T {
  if (value && typeof value === "object") {
    if (!Array.isArray(value) && ![Object.prototype, null].includes(Object.getPrototypeOf(value))) {
      throw new SyncError("INVALID_UPDATE");
    }
    for (const child of Object.values(value)) freezeJson(child);
    Object.freeze(value);
  }
  return value;
}

/** One immutable request in one joined instance. Ending tracking is not cancellation. */
export class TrackedEntityCommand {
  readonly #request: ConfirmedCommandRequest;
  private latest: Promise<CommandResult> = Promise.resolve({ status: "ended", commandId: "", code: "NOT_SENT", detail: "Not sent" });
  private snapshot: CommandState;
  constructor(private readonly owner: ConfirmedEntityCommands, request: ConfirmedCommandRequest) {
    this.#request = request;
    this.snapshot = Object.freeze({ commandId: request.commandId, entityId: request.entityId, operation: request.operation, attempts: 0, status: "pending" });
  }
  get request(): ConfirmedCommandRequest { return this.#request; }
  get state(): CommandState { return this.snapshot; }
  /** Result of the most recent explicit attempt; late receipts also update state. */
  get result(): Promise<CommandResult> { return this.latest; }
  retry(options: { timeoutMs?: number } = {}): Promise<CommandResult> { return this.owner._retry(this, options.timeoutMs); }
  end(): void { this.owner._end(this); }
  _state(state: CommandState): void { this.snapshot = Object.freeze(state); }
  _attempt(result: Promise<CommandResult>): void { this.latest = result; }
}

/** Bounded receipt tracking. Canonical snapshots never prove command execution. */
export class ConfirmedEntityCommands {
  private readonly entries = new Map<string, Entry>();
  private readonly listeners = new Set<(state: CommandState) => void>();
  private retainedBytes = 0;
  private closed = false;
  constructor(private readonly transport: Transport) {}

  get pending(): ReadonlyArray<CommandState> { return [...this.entries.values()].map(entry => entry.handle.state); }
  get metrics(): Readonly<{ pending: number; bytes: number }> { return Object.freeze({ pending: this.entries.size, bytes: this.retainedBytes }); }
  /** Local lookup of unresolved tracking; completed results remain on their handle. */
  get(commandId: string): TrackedEntityCommand | undefined { return this.entries.get(commandId)?.handle; }
  onChange(listener: (state: CommandState) => void): () => void {
    if (this.listeners.size >= 128 && !this.listeners.has(listener)) throw new SyncError("RESOURCE_LIMIT");
    this.listeners.add(listener); return () => { this.listeners.delete(listener); };
  }

  submit(options: Omit<EntityCommandOptions, "commandId">, wait: { timeoutMs?: number } = {}): TrackedEntityCommand {
    this.requireReady();
    const timeout = this.timeout(wait.timeoutMs);
    if (!options.entityId || options.entityId.length > 256 || !options.operation || options.operation.length > 256) throw new SyncError("INVALID_UPDATE");
    stateBytes(options.args ?? {}, 65536, false);
    const request = freezeJson({ entityId: options.entityId, operation: options.operation,
      expectedRevision: uint64(options.expectedRevision ?? this.transport.entityRevision(options.entityId) ?? 0n, "INVALID_UPDATE"),
      args: structuredClone(options.args ?? {}), commandId: uuidv7() });
    const bytes = stateBytes(request, 65536);
    if (this.entries.has(request.commandId) || this.entries.size >= CONFIRMED_COMMAND_MAX_PENDING
      || bytes > CONFIRMED_COMMAND_MAX_BYTES - this.retainedBytes) throw new SyncError("RESOURCE_LIMIT");
    const handle = new TrackedEntityCommand(this, request);
    const entry: Entry = { handle, request, bytes, attempts: 0, unknown: false };
    this.entries.set(request.commandId, entry); this.retainedBytes += bytes;
    this.send(entry, timeout);
    return handle;
  }

  _retry(handle: TrackedEntityCommand, timeoutMs?: number): Promise<CommandResult> {
    this.requireReady();
    const timeout = this.timeout(timeoutMs), entry = this.entries.get(handle.request.commandId);
    if (!entry || entry.handle !== handle || entry.waiter || entry.result?.status !== "unknown") throw new Error("Only a retained unknown command can be retried");
    this.send(entry, timeout);
    return handle.result;
  }
  _end(handle: TrackedEntityCommand): void {
    const entry = this.entries.get(handle.request.commandId);
    if (entry?.handle === handle) this.finish(entry, { status: "ended", commandId: entry.request.commandId, code: "TRACKING_ENDED", detail: "Tracking ended; a sent command may still commit" });
  }
  _stop(closed: boolean, code: string): void {
    if (closed) this.closed = true;
    for (const entry of [...this.entries.values()]) {
      this.finish(entry, { status: closed ? "ended" : "unknown", commandId: entry.request.commandId, code,
        detail: closed ? "Instance closed; a sent command may still commit" : "Transport changed; execution is unknown" });
    }
    if (closed) this.listeners.clear();
  }
  _receipt(command: EntityCommand): void {
    const entry = this.entries.get(command.commandId);
    if (!entry || command.entityId !== entry.request.entityId || command.instanceRevision === undefined) return;
    if (entry.request.operation !== "input" && command.operation !== entry.request.operation) return;
    this.finish(entry, { status: "committed", commandId: command.commandId,
      entityRevision: command.expectedRevision, instanceRevision: command.instanceRevision });
  }
  _error(error: { requestMessageId?: string; code: string; message?: string; retryable?: boolean }): void {
    const entry = error.requestMessageId ? this.entries.get(error.requestMessageId) : undefined;
    if (!entry) return;
    const unknown = entry.unknown || error.retryable || ["PERSISTENCE_UNAVAILABLE", "COMMAND_TIMEOUT", "COMMAND_RETRY", "REPLAY_WINDOW_EXPIRED"].includes(error.code);
    this.finish(entry, { status: unknown ? "unknown" : "rejected", commandId: entry.request.commandId, code: error.code,
      detail: unknown ? "Execution remains unknown; a retry rejection does not disprove the original commit" : error.message ?? error.code });
  }

  private requireReady(): void {
    if (this.closed) throw new SyncError("CLOSED");
    if (this.transport.syncStatus !== "ready") throw new SyncError("NOT_READY");
  }
  private timeout(value = 10_000): number {
    if (!Number.isSafeInteger(value) || value <= 0 || value > 2_147_483_647) throw new SyncError("INVALID_UPDATE");
    return value;
  }
  private send(entry: Entry, timeout: number): void {
    entry.attempts++;
    entry.result = undefined;
    const result = new Promise<CommandResult>(resolve => {
      const timer = setTimeout(() => this.finish(entry, { status: "unknown", commandId: entry.request.commandId,
        code: "CONFIRMATION_TIMEOUT", detail: "Receipt not observed; execution is unknown" }), timeout);
      (timer as unknown as { unref?: () => void }).unref?.();
      entry.waiter = { resolve, timer };
    });
    entry.handle._attempt(result);
    this.publish(entry);
    if (!entry.waiter) return; // A listener may explicitly end tracking.
    try { this.transport.sendEntityCommand(entry.request); }
    catch { this.finish(entry, { status: "unknown", commandId: entry.request.commandId, code: "SEND_UNCERTAIN", detail: "Send did not establish execution or rejection" }); }
  }
  private finish(entry: Entry, result: CommandResult): void {
    if (this.entries.get(entry.request.commandId) !== entry) return;
    const waiter = entry.waiter;
    if (waiter) clearTimeout(waiter.timer);
    entry.waiter = undefined;
    const outcome = Object.freeze(result);
    entry.result = outcome;
    this.transport._forgetQueuedCommand(entry.request.commandId);
    if (result.status === "unknown") entry.unknown = true;
    else { this.entries.delete(entry.request.commandId); this.retainedBytes -= entry.bytes; }
    this.publish(entry);
    waiter?.resolve(outcome);
  }
  private publish(entry: Entry): void {
    entry.handle._state({ commandId: entry.request.commandId, entityId: entry.request.entityId,
      operation: entry.request.operation, attempts: entry.attempts, status: entry.result?.status ?? "pending", ...(entry.result ? { result: entry.result } : {}) });
    for (const listener of [...this.listeners]) { try { listener(entry.handle.state); } catch { /* Listeners cannot interrupt tracking. */ } }
  }
}
