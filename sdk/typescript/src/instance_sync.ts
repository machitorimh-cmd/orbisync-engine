import { clone, toBinary } from "@bufbuild/protobuf";
import { EnvelopeSchema, type Envelope, type Snapshot } from "./generated/orbisync/v1/realtime_pb.js";
import { SnapshotAssembler, SnapshotAssemblyError, type SnapshotAssemblyLimits } from "./snapshot_assembler.js";
import { SyncError, SyncStore, equalState, type InstanceState, type StateEvent } from "./sync_state.js";

export type SyncStatus = "syncing" | "ready" | "reconnecting" | "recovering" | "failed" | "closed";
export type SyncOptions = Partial<SnapshotAssemblyLimits> & { maxPendingMessages?: number; maxPendingBytes?: number; maxStateBytes?: number };
export type ReadyOptions = { timeoutMs?: number; signal?: AbortSignal };
type Waiter = { resolve: () => void; reject: (error: SyncError) => void };

/** Internal generation-scoped lifecycle; application callbacks are isolated by the instance. */
export class InstanceSync {
  private store = new SyncStore();
  private assembler: SnapshotAssembler | null = null;
  private pending: Envelope[] = [];
  private pendingBytes = 0;
  private waiters = new Set<Waiter>();
  private timer: ReturnType<typeof setTimeout> | null = null;
  private completed = new Map<string, { revision: bigint; count: number }>();
  private lastSnapshot: { id: string; data: Uint8Array; lengths: number[] } | null = null;
  private generation = 0;
  private lifecycle = 0;
  private initialDeadline = 0;
  private failure: SyncError | null = null;
  private receivedSnapshot = false;
  private supported = false;
  status: SyncStatus = "reconnecting";

  constructor(
    private readonly instanceId: string,
    private readonly emit: (event: string, payload: unknown) => void,
    private readonly recover: (error: SyncError) => void,
    private readonly options: SyncOptions = {},
  ) {
    for (const value of Object.values(options)) {
      if (!Number.isSafeInteger(value) || value <= 0) throw new SyncError("RESOURCE_LIMIT");
    }
  }

  get enabled(): boolean { return this.supported; }
  get state(): InstanceState { return this.store.state; }
  get revision(): bigint { return this.store.revision; }
  entityRevision(entityId: string): bigint | undefined { return this.store.entityRevision(entityId); }

  begin(generation: number, supported: boolean): void {
    const lifecycle = ++this.lifecycle;
    this.clearPending();
    if (this.status === "ready" || this.status === "syncing") this.settle(new SyncError("DISCONNECTED"));
    this.generation = generation;
    this.supported = supported;
    this.failure = null;
    this.completed.clear();
    this.lastSnapshot = null;
    this.receivedSnapshot = false;
    if (!supported) {
      this.failure = new SyncError("UNSUPPORTED_SERVER");
      this.settle(this.failure);
      this.changeStatus("failed");
      return;
    }
    this.assembler = new SnapshotAssembler(this.instanceId, generation, this.options);
    this.initialDeadline = Date.now() + (this.options.timeoutMs ?? 10_000);
    this.changeStatus("syncing");
    if (lifecycle === this.lifecycle) this.armTimer();
  }

  ready(options: ReadyOptions = {}): Promise<void> {
    if (options.signal?.aborted) return Promise.reject(new SyncError("ABORTED"));
    if (this.status === "ready") return Promise.resolve();
    if (this.status === "failed" || this.status === "closed") return Promise.reject(this.failure ?? new SyncError("CLOSED"));
    const timeoutMs = options.timeoutMs ?? 10_000;
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs <= 0) return Promise.reject(new SyncError("SYNC_TIMEOUT"));
    if (this.waiters.size >= 1024) return Promise.reject(new SyncError("RESOURCE_LIMIT"));
    return new Promise((resolve, reject) => {
      let timer: ReturnType<typeof setTimeout>;
      const cleanup = (): void => {
        clearTimeout(timer);
        options.signal?.removeEventListener("abort", abort);
        this.waiters.delete(waiter);
      };
      const waiter: Waiter = {
        resolve: () => { cleanup(); resolve(); },
        reject: error => { cleanup(); reject(error); },
      };
      const abort = (): void => waiter.reject(new SyncError("ABORTED"));
      timer = setTimeout(() => waiter.reject(new SyncError("SYNC_TIMEOUT")), timeoutMs);
      this.waiters.add(waiter);
      options.signal?.addEventListener("abort", abort, { once: true });
      if (options.signal?.aborted) abort();
    });
  }

  stop(error: SyncError, status: "reconnecting" | "failed" | "closed" = "reconnecting"): void {
    ++this.lifecycle;
    this.clearPending();
    this.failure = error;
    this.completed.clear();
    this.lastSnapshot = null;
    if (status === "closed") this.store = new SyncStore();
    this.settle(error);
    this.changeStatus(status);
  }

  private changeStatus(status: SyncStatus): void {
    if (this.status === status) return;
    this.status = status;
    this.emit("syncStateChanged", status);
  }

  receive(envelope: Envelope): void {
    if (!this.supported || this.status === "closed" || this.status === "failed" || this.status === "recovering" || this.status === "reconnecting") return;
    const payload = envelope.payload;
    try {
      if (envelope.instanceId && envelope.instanceId !== this.instanceId) throw new SyncError("INVALID_UPDATE");
      if (payload.case === "snapshot") { this.snapshot(payload.value, envelope.instanceId); return; }
      if (payload.case === "resumeAccepted" && !payload.value.replayFollows) {
        // The current Core sends a snapshot. Support no-replay only when an
        // actually retained committed state already matches the offered boundary.
        if (this.store.boundary > 0n && this.store.revision === payload.value.currentRevision) {
          this.receivedSnapshot = true;
          this.clearTimer();
          this.settle();
          this.changeStatus("ready");
        } else throw new SyncError("INVALID_UPDATE");
        return;
      }
      if (payload.case !== "stateDelta" && payload.case !== "entityCommand") return;
      if (envelope.instanceId !== this.instanceId) throw new SyncError("INVALID_UPDATE");
      if (this.status !== "ready" || (this.assembler?.usage.snapshots ?? 0) > 0) {
        const size = toBinary(EnvelopeSchema, envelope).byteLength;
        if (this.pending.length >= (this.options.maxPendingMessages ?? 2048)
          || size > (this.options.maxPendingBytes ?? 4 * 1024 * 1024) - this.pendingBytes) throw new SyncError("RESOURCE_LIMIT");
        this.pending.push(clone(EnvelopeSchema, envelope));
        this.pendingBytes += size;
        return;
      }
      const events = this.store.transaction(staged => this.apply(staged, envelope));
      this.publish(events, envelope);
    } catch (error) { this.fail(error); }
  }

  private snapshot(chunk: Snapshot, instanceId: string): void {
    const lifecycle = this.lifecycle;
    // Even completed IDs must not be silently reused with contradictory metadata.
    const completed = this.completed.get(chunk.snapshotId);
    if (completed !== undefined) {
      if (instanceId !== this.instanceId || completed.revision !== chunk.instanceRevision || completed.count !== chunk.chunkCount
        || !Number.isInteger(chunk.chunkIndex) || chunk.chunkIndex < 0 || chunk.chunkIndex >= chunk.chunkCount) throw new SyncError("REVISION_CONFLICT");
      if (this.lastSnapshot?.id === chunk.snapshotId) {
        const offset = this.lastSnapshot.lengths.slice(0, chunk.chunkIndex).reduce((sum, length) => sum + length, 0);
        if (this.lastSnapshot.lengths[chunk.chunkIndex] !== chunk.data.length
          || chunk.data.some((byte, index) => byte !== this.lastSnapshot!.data[offset + index])) throw new SyncError("REVISION_CONFLICT");
      }
      // Already verified older snapshots are stale by the committed floor.
      return;
    }
    if (this.status === "ready") {
      this.initialDeadline = Date.now() + (this.options.timeoutMs ?? 10_000);
      this.changeStatus("syncing");
      if (lifecycle !== this.lifecycle) return;
    }
    const result = this.assembler!.accept({ chunk, instanceId, generation: this.generation }, Date.now());
    if (result.status !== "complete") { this.armTimer(); return; }
    const assembled = result.snapshot;
    const staged = SyncStore.snapshot(assembled.data, assembled.instanceRevision, this.instanceId, this.options.maxStateBytes);
    if (this.receivedSnapshot && staged.boundary < this.store.boundary) {
      if (this.assembler!.usage.snapshots === 0) this.finishReady();
      return;
    }
    if (this.receivedSnapshot && staged.boundary === this.store.boundary) {
      // A repeated baseline may precede already-applied deltas. Keep the newer
      // state; completed-snapshot byte validation is handled below by baseline.
      if (this.baseline !== null && !equalState(this.baseline, this.snapshotContent(staged.state))) throw new SyncError("REVISION_CONFLICT");
      if (this.assembler!.usage.snapshots === 0) this.finishReady();
      return;
    }
    // A delayed snapshot can be newer than the baseline yet older than a live
    // update already committed by this generation. It cannot replace that state.
    if (this.receivedSnapshot && staged.boundary < this.store.revision) {
      if (this.assembler!.usage.snapshots === 0) this.finishReady();
      return;
    }
    const baseline = this.snapshotContent(staged.state);
    const events: Array<{ events: StateEvent[]; envelope: Envelope }> = [];
    const revision = (env: Envelope): bigint => env.payload.case === "stateDelta" ? env.payload.value.toRevision
      : env.payload.case === "entityCommand" ? env.payload.value.instanceRevision ?? -1n : -1n;
    for (const env of [...this.pending].sort((a, b) => revision(a) < revision(b) ? -1 : revision(a) > revision(b) ? 1 : 0)) {
      events.push({ events: this.apply(staged, env), envelope: env });
    }
    this.store = staged;
    this.baseline = baseline;
    this.receivedSnapshot = true;
    this.pending = [];
    this.pendingBytes = 0;
    this.completed.set(assembled.snapshotId, { revision: assembled.instanceRevision, count: assembled.chunkLengths.length });
    this.lastSnapshot = { id: assembled.snapshotId, data: assembled.data, lengths: assembled.chunkLengths };
    if (this.completed.size > 64) this.completed.delete(this.completed.keys().next().value!);
    this.assembler!.discardOlderThan(assembled.instanceRevision);
    if (this.assembler!.usage.snapshots === 0) this.finishReady(); else this.armTimer();
    if (lifecycle !== this.lifecycle) return;
    this.emit("snapshot", { ...chunk, chunkIndex: 0, chunkCount: 1, data: assembled.data.slice() });
    for (const item of events) {
      if (lifecycle !== this.lifecycle) return;
      this.publish(item.events, item.envelope);
    }
  }

  private baseline: unknown = null;
  private snapshotContent(state: InstanceState): unknown {
    return { entities: [...state.entities].sort(([a], [b]) => a.localeCompare(b)),
      presences: [...state.presences].sort(([a], [b]) => a.localeCompare(b)) };
  }

  private apply(store: SyncStore, envelope: Envelope): StateEvent[] {
    return envelope.payload.case === "stateDelta" ? store.applyDelta(envelope.payload.value)
      : envelope.payload.case === "entityCommand" ? store.applyCommand(envelope.payload.value) : [];
  }

  private publish(events: StateEvent[], envelope: Envelope): void {
    const lifecycle = this.lifecycle;
    for (const event of events) {
      if (lifecycle !== this.lifecycle) return;
      this.emit(event.event, event.payload);
    }
    if (lifecycle !== this.lifecycle) return;
    // Confirmation also acknowledges an idempotent replay. It must never apply
    // a mutation twice, but pending callers still need the correlated receipt.
    if (envelope.payload.case === "entityCommand") this.emit("entityCommand", envelope.payload.value);
  }

  private finishReady(): void {
    const lifecycle = this.lifecycle;
    if (this.pending.length > 0) {
      const events = this.store.transaction(staged =>
        this.pending.map(envelope => ({ envelope, events: this.apply(staged, envelope) })));
      this.pending = [];
      this.pendingBytes = 0;
      for (const item of events) {
        if (lifecycle !== this.lifecycle) return;
        this.publish(item.events, item.envelope);
      }
    }
    if (lifecycle !== this.lifecycle) return;
    this.clearTimer();
    this.failure = null;
    this.settle();
    this.changeStatus("ready");
  }

  private armTimer(): void {
    this.clearTimer();
    const deadline = Math.min(this.initialDeadline, this.assembler?.nextDeadline ?? Infinity);
    this.timer = setTimeout(() => this.fail(new SyncError("SYNC_TIMEOUT")), Math.max(0, deadline - Date.now()));
    (this.timer as unknown as { unref?: () => void }).unref?.();
  }
  private clearTimer(): void { if (this.timer !== null) clearTimeout(this.timer); this.timer = null; }
  private clearPending(): void {
    this.clearTimer();
    this.assembler?.dispose();
    this.assembler = null;
    this.completed.clear();
    this.lastSnapshot = null;
    this.baseline = null;
    this.pending = [];
    this.pendingBytes = 0;
  }
  private settle(error?: SyncError): void {
    for (const waiter of [...this.waiters]) { if (error) waiter.reject(error); else waiter.resolve(); }
  }
  fail(error: unknown): void {
    if (this.status === "closed" || this.status === "failed" || this.status === "recovering") return;
    const failure = error instanceof SyncError ? error : error instanceof SnapshotAssemblyError
      ? new SyncError(error.code === "RESOURCE_LIMIT" ? "RESOURCE_LIMIT" : error.code === "SNAPSHOT_TIMEOUT" ? "SYNC_TIMEOUT" : "INVALID_SNAPSHOT")
      : new SyncError("INVALID_UPDATE");
    const lifecycle = ++this.lifecycle;
    this.clearPending();
    this.failure = failure;
    this.settle(failure);
    this.changeStatus(failure.code === "UNSUPPORTED_SERVER" ? "failed" : "recovering");
    if (lifecycle !== this.lifecycle) return;
    this.emit("error", failure);
    if (lifecycle === this.lifecycle) this.recover(failure);
  }
}
