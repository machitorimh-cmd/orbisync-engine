import { uuidv7, type OrbiSyncInstance } from "../../../sdk/typescript/src/client.js";
import type { EntityCommand } from "../../../sdk/typescript/src/generated/orbisync/v1/realtime_pb.js";
import { stateBytes, uint64 } from "../../../sdk/typescript/src/sync_state.js";
type Input = Parameters<OrbiSyncInstance["sendEntityCommand"]>[0];
export type Request = Input & { commandId: string; expectedRevision: bigint };
export class CommandUncertain extends Error {
  readonly outcome = "uncertain";
  constructor(readonly code: string, message: string) { super(message); this.name = "CommandUncertain"; }
}
export class CommandRejected extends Error {
  readonly outcome = "rejected";
  constructor(readonly code: string, message: string) { super(message); this.name = "CommandRejected"; }
}
export class CommandInterrupted extends Error {
  readonly outcome = "interrupted";
  readonly code = "CLOSED";
  constructor() { super("退室により追跡を終了しました。送信済み操作の取消ではありません"); this.name = "CommandInterrupted"; }
}
type Waiter = { resolve: (command: EntityCommand) => void; reject: (error: Error) => void; timer: ReturnType<typeof setTimeout> };
type RecordEntry = { request: Request; bytes: number; error?: CommandUncertain; waiter?: Waiter };
function freeze<T>(value: T): T {
  if (value && typeof value === "object") { for (const child of Object.values(value)) freeze(child); Object.freeze(value); }
  return value;
}
/** Correlation only. Ready/state never substitutes for a durable command receipt. */
export class ConfirmedCommands {
  private pendingBytes = 0;
  private pending = new Map<string, RecordEntry>();
  private disposed = false;
  private issued = new WeakSet<Request>();
  constructor(private readonly instance: OrbiSyncInstance,
    private readonly changed: () => void = () => {},
    private readonly settled: (command: EntityCommand, request: Request) => void = () => {}) {
    instance.on("entityCommand", this.confirm);
    instance.on("error", this.error);
    instance.on("syncStateChanged", this.status);
  }
  get uncertain(): ReadonlyArray<{ request: Request; error: CommandUncertain; busy: boolean }> {
    return [...this.pending.values()].filter(entry => entry.error).map(entry => ({ request: entry.request, error: entry.error!, busy: !!entry.waiter }));
  }
  hasPending(entityId: string): boolean {
    return [...this.pending.values()].some(entry => entry.request.entityId === entityId);
  }
  get unresolvedCount(): number { return this.pending.size; }
  request(options: Omit<Input, "commandId">): Request {
    stateBytes(options, 65536);
    const expectedRevision = uint64(options.expectedRevision ?? this.instance.state.entities.get(options.entityId)?.revision ?? 0n, "INVALID_UPDATE");
    const request = freeze({ ...structuredClone(options), expectedRevision, commandId: uuidv7() });
    this.issued.add(request);
    return request;
  }
  send(request: Request): Promise<EntityCommand> {
    if (this.disposed) return Promise.reject(new CommandRejected("CLOSED", "退室しました"));
    if (!this.issued.has(request)) return Promise.reject(new CommandRejected("REQUEST_SCOPE", "別の入室で作成した操作は送信できません"));
    let entry = this.pending.get(request.commandId);
    if (entry && entry.request !== request) return Promise.reject(new CommandRejected("COMMAND_ID_CONFLICT", "保持した同じ操作を再送してください"));
    if (entry?.waiter) return Promise.reject(new Error("この操作は確定を待っています"));
    if (!entry) {
      if (this.hasPending(request.entityId)) return Promise.reject(new CommandRejected("OPERATION_PENDING", "この付箋の未確定操作を先に確認してください"));
      let bytes: number;
      try { bytes = stateBytes(request, 65536); } catch (error) { return Promise.reject(error); }
      if (this.pending.size >= 32 || bytes > 1024 * 1024 - this.pendingBytes) return Promise.reject(new CommandRejected("RESOURCE_LIMIT", "未確定操作の上限です。保持した操作を先に確認してください"));
      freeze(request);
      entry = { request, bytes };
      this.pending.set(request.commandId, entry); this.pendingBytes += bytes;
    }
    const retained = entry;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this.finish(request.commandId, new CommandUncertain("CONFIRMATION_TIMEOUT", "確定を確認できません。同じ操作を再送できます")), 10000);
      retained.waiter = { resolve, reject, timer };
      this.changed();
      try { this.instance.sendEntityCommand(request); }
      catch (error) {
        // A synchronous transport send exception cannot prove the server did not apply it.
        this.finish(request.commandId, new CommandUncertain("SEND_UNCERTAIN", error instanceof Error ? error.message : String(error)));
      }
    });
  }
  private finish(id: string, result: EntityCommand | Error): void {
    const entry = this.pending.get(id);
    if (!entry) return;
    const waiter = entry.waiter;
    if (waiter) clearTimeout(waiter.timer);
    entry.waiter = undefined;
    if (result instanceof CommandUncertain) entry.error = result;
    else { this.pending.delete(id); this.pendingBytes -= entry.bytes; }
    if (result instanceof Error) waiter?.reject(result);
    else { waiter?.resolve(result); this.settled(result, entry.request); }
    this.changed();
  }
  private confirm = (raw: unknown): void => {
    const command = raw as EntityCommand, entry = this.pending.get(command.commandId);
    if (!entry || command.entityId !== entry.request.entityId || command.operation !== entry.request.operation) return;
    this.finish(command.commandId, command);
  };
  private error = (raw: unknown): void => {
    const error = raw as { requestMessageId?: string; code?: string; message?: string; retryable?: boolean };
    if (!error.requestMessageId) return;
    const entry = this.pending.get(error.requestMessageId);
    if (!entry) return;
    const code = error.code ?? "UNKNOWN";
    const uncertain = error.retryable || ["PERSISTENCE_UNAVAILABLE", "COMMAND_TIMEOUT", "COMMAND_RETRY"].includes(code);
    // After an uncertain attempt, a rejection can be a new admission following
    // lost/expired dedup (e.g. revision mismatch after restart). The wire does not
    // distinguish that from replay of an original rejected outcome. It cannot
    // establish that the original mutation was rejected or rolled back.
    if (entry.error && !uncertain) {
      this.finish(error.requestMessageId, new CommandUncertain(code, `${code}: 再送は拒否されましたが、元の操作の結果は未確認です。再同期した状態は保存確定の代用にはなりません。`));
      return;
    }
    this.finish(error.requestMessageId, uncertain ? new CommandUncertain(code, error.message ?? code) : new CommandRejected(code, `${code}: ${error.message ?? ""}`));
  };
  private status = (status: unknown): void => {
    if (status === "closed") { this.dispose(); return; }
    if (status !== "ready") for (const id of [...this.pending.keys()]) this.finish(id, new CommandUncertain("DISCONNECTED", "接続が変わりました。同期完了後に再送できます"));
  };
  dispose(): void {
    this.disposed = true;
    for (const id of [...this.pending.keys()]) this.finish(id, new CommandInterrupted());
    this.instance.off("entityCommand", this.confirm); this.instance.off("error", this.error); this.instance.off("syncStateChanged", this.status);
  }
}
