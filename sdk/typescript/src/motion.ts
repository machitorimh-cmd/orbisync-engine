import type { InputOptions, InputResult, OrbiSyncInstance, SyncedEntity } from "./client.js";

type Request = InputOptions & { commandId: string; expectedRevision: bigint };

/** One controller per locally controlled entity. No automatic queue or retries. */
export class PredictedInput<T> {
  private pending: { request: Request; predicted: T; revision: bigint; predicting: boolean } | undefined;
  private outcome: InputResult | undefined;
  private disposed = false;
  private readonly onStatus = (status: unknown): void => {
    if (status !== "ready" && this.pending) this.pending.predicting = false;
  };

  constructor(
    private readonly instance: OrbiSyncInstance,
    private readonly entityId: string,
    private readonly rule: string,
    private readonly read: (entity: SyncedEntity) => T,
    private readonly predict: (state: T, intent: Record<string, unknown>) => T,
  ) {
    instance.on("syncStateChanged", this.onStatus);
  }

  /** Detached display data; neither policies nor consumers can mutate canonical state. */
  get authoritative(): T | undefined {
    const entity = this.instance.state.entities.get(this.entityId);
    return entity ? structuredClone(this.read(structuredClone(entity))) : undefined;
  }
  get display(): T | undefined {
    const entity = this.instance.state.entities.get(this.entityId);
    // Any authoritative advance invalidates this prediction, including our own
    // receipt before its promise callback. Never apply an intent twice.
    if (this.instance.syncStatus === "ready" && this.pending?.predicting
      && entity?.revision === this.pending.revision) return structuredClone(this.pending.predicted);
    return this.authoritative;
  }
  get pendingRequest(): Request | undefined { return this.pending && structuredClone(this.pending.request); }
  get lastResult(): InputResult | undefined { return this.outcome && { ...this.outcome }; }

  submit(intent: Record<string, unknown>, timeoutMs?: number): Promise<InputResult> {
    this.requireReady();
    if (this.pending) throw new Error("An input is pending; await its receipt or resolve uncertainty first");
    const entity = this.instance.state.entities.get(this.entityId);
    if (!entity) throw new Error("Controlled entity is absent from canonical state");
    const predicted = structuredClone(this.predict(this.authoritative!, structuredClone(intent)));
    const submission = this.instance.sendInput({ entityId: this.entityId, rule: this.rule,
      intent, expectedRevision: entity.revision, timeoutMs });
    this.pending = { request: structuredClone(submission.request), predicted,
      revision: entity.revision, predicting: true };
    this.outcome = undefined;
    return this.observe(submission);
  }

  /** Explicit retry uses the original ID, intent and revision, with no prediction. */
  retry(): Promise<InputResult> {
    this.requireReady();
    if (!this.pending || this.outcome?.status !== "uncertain") throw new Error("No uncertain input to retry");
    const submission = this.instance.sendInput(structuredClone(this.pending.request));
    this.outcome = undefined;
    return this.observe(submission);
  }

  /** Explicitly stop tracking uncertainty, without resending the intent.
   * A timed-out command can still commit later; canonical sync remains truth.
   */
  useCanonical(): void {
    this.requireReady();
    if (this.pending && this.outcome?.status !== "uncertain") throw new Error("Input receipt is still pending");
    this.pending = undefined;
  }

  dispose(): void {
    this.disposed = true;
    if (this.pending) this.pending.predicting = false;
    this.instance.off("syncStateChanged", this.onStatus);
  }

  private requireReady(): void {
    if (this.disposed || this.instance.syncStatus !== "ready") throw new Error("Prediction requires a live, ready instance");
  }
  private async observe(submission: { request: Request; result: Promise<InputResult> }): Promise<InputResult> {
    const result = await submission.result;
    this.outcome = result;
    if (result.status === "uncertain") {
      if (this.pending) this.pending.predicting = false;
    } else this.pending = undefined;
    return result;
  }
}

/** Bounded authoritative sample buffer. Times use one caller-owned monotonic clock.
 * Supply application interpolation (e.g. vector lerp); no extrapolation or physics.
 */
export class RemoteInterpolator<T> {
  private samples: Array<{ revision: bigint; time: number; value: T }> = [];
  constructor(private readonly interpolate: (a: T, b: T, fraction: number) => T,
    readonly delayMs = 100, private readonly capacity = 32) {
    if (!Number.isFinite(delayMs) || delayMs < 0 || !Number.isSafeInteger(capacity) || capacity < 2)
      throw new Error("Invalid interpolation options");
  }
  push(revision: bigint, time: number, value: T): boolean {
    if (!Number.isFinite(time)) throw new Error("Invalid sample time");
    const last = this.samples.at(-1);
    if (last && (revision <= last.revision || time < last.time)) return false;
    if (last?.time === time) this.samples.pop();
    this.samples.push({ revision, time, value: structuredClone(value) });
    if (this.samples.length > this.capacity) this.samples.shift();
    return true;
  }
  sample(now: number): T | undefined {
    if (!Number.isFinite(now)) throw new Error("Invalid display time");
    const target = now - this.delayMs;
    if (!this.samples.length) return undefined;
    for (let i = 1; i < this.samples.length; i++) {
      const a = this.samples[i - 1], b = this.samples[i];
      if (target < b.time) return target <= a.time ? structuredClone(a.value)
        : this.interpolate(structuredClone(a.value), structuredClone(b.value), (target - a.time) / (b.time - a.time));
    }
    return structuredClone(this.samples.at(-1)!.value);
  }
  /** Clear on reconnect/snapshot replacement, deletion, or interest re-entry. */
  reset(): void { this.samples = []; }
}
