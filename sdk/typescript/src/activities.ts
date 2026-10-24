import { fromJson, toBinary } from "@bufbuild/protobuf";
import { StructSchema } from "@bufbuild/protobuf/wkt";
import { ActivitySnapshotSchema, ActivityChangedSchema, type ActivityEntry, type ActivitySnapshot, type ActivityChanged } from "./generated/orbisync/v1/realtime_pb.js";
import { SyncError, uint64 } from "./sync_state.js";
import { activityKey, canonicalId, ownJson, type RealtimeJsonObject } from "./realtime_features.js";
import { uuidv7 } from "./uuidv7.js";

export type Activity = Readonly<{
  presenceId: string; userId: string; key: string; data: RealtimeJsonObject; expiresAtUnixMs: bigint;
}>;
export type ActivityOptions = { key: string; data?: Record<string, unknown>; ttlMs?: number };
type Transport = {
  check(): void;
  update(key: string, data: RealtimeJsonObject, ttlMs: number): void;
  requestSnapshot(requestId: string): void;
  ownPresenceId(): string | undefined;
  now(): number;
  error(error: SyncError): void;
};
const MAX_BYTES = 512 * 1024;
const MAX_ENTRIES = 4096;
const TIMEOUT_MS = 10_000;
function unref(timer: ReturnType<typeof setTimeout>): void { (timer as unknown as { unref?(): void }).unref?.(); }
function identity(activity: Activity): string { return `${activity.presenceId}:${activity.key}`; }
function activityData(value: unknown): RealtimeJsonObject {
  const result = ownJson(value, 32 * 1024);
  let nodes = 0;
  const visit = (item: unknown, depth: number): void => {
    if (depth > 16 || ++nodes > 1024) throw new SyncError("RESOURCE_LIMIT");
    if (item && typeof item === "object") for (const child of Object.values(item)) visit(child, depth + 1);
  };
  for (const child of Object.values(result)) visit(child, 1);
  if (toBinary(StructSchema, fromJson(StructSchema, result as never)).length > 4096) throw new SyncError("RESOURCE_LIMIT");
  return result;
}

/** Authenticated, expiring application state. Updates are never queued or replayed. */
export class InstanceActivities {
  private values = new Map<string, Activity>();
  private view: readonly Activity[] = Object.freeze([]);
  private readonly listeners = new Set<(activities: readonly Activity[]) => void>();
  private version = 0n;
  private initialized = false;
  private requested = false;
  private pending = false;
  private requestId: string | null = null;
  private buffered = new Map<bigint, { activity: Activity; removed: boolean; bytes: number }>();
  private bufferedBytes = 0;
  private assembly: { id: string; version: bigint; count: number; bytes: number; chunks: Map<number, { bytes: Uint8Array; values: Activity[] }> } | null = null;
  private deadline: ReturnType<typeof setTimeout> | null = null;
  private expiry: ReturnType<typeof setTimeout> | null = null;
  private readyRetry: ReturnType<typeof setTimeout> | null = null;
  private readyDeadline = 0;
  constructor(private readonly transport: Transport) {}

  get current(): readonly Activity[] { return this.view; }
  onChange(listener: (activities: readonly Activity[]) => void): () => void {
    if (!this.listeners.has(listener) && this.listeners.size >= 128) throw new SyncError("RESOURCE_LIMIT");
    this.listeners.add(listener); return () => { this.listeners.delete(listener); };
  }
  set(options: ActivityOptions): void {
    this.transport.check();
    const key = activityKey(options.key), ttl = options.ttlMs ?? 5000;
    if (!Number.isInteger(ttl) || ttl < 250 || ttl > 30_000) throw new SyncError("INVALID_UPDATE");
    const own = this.transport.ownPresenceId();
    if (!this.values.has(`${own}:${key}`) && [...this.values.values()].filter(item => item.presenceId === own).length >= 16) throw new SyncError("RESOURCE_LIMIT");
    this.transport.update(key, activityData(options.data ?? {}), ttl);
  }
  clear(key: string): void { this.transport.check(); this.transport.update(activityKey(key), ownJson({}, 4096), 0); }
  /** Retry a failed activity snapshot explicitly; canonical Entity state is unaffected. */
  refresh(): void {
    this.transport.check();
    if (this.pending) return;
    this.requested = true; this.pending = true;
    this.requestId = uuidv7();
    this.deadline = setTimeout(() => this.fail(new SyncError("SYNC_TIMEOUT")), TIMEOUT_MS); unref(this.deadline);
    try { this.transport.requestSnapshot(this.requestId); } catch (error) { this.cancelPending(); this.requested = false; throw error; }
  }
  _ready(): void {
    if (this.requested) return;
    try { this.refresh(); } catch (error) {
      if (!(error instanceof SyncError) || error.code === "UNSUPPORTED_SERVER") return;
      if (["NOT_READY", "RESOURCE_LIMIT"].includes(error.code)) {
        this.readyDeadline ||= Date.now() + TIMEOUT_MS;
        if (Date.now() >= this.readyDeadline) { this.transport.error(new SyncError("SYNC_TIMEOUT")); return; }
        if (!this.readyRetry) {
          this.readyRetry = setTimeout(() => { this.readyRetry = null; this._ready(); }, 50); unref(this.readyRetry);
        }
      } else this.transport.error(error);
    }
  }
  _reset(closed = false): void {
    this.cancelPending(); this.requested = false; this.initialized = false; this.version = 0n;
    if (this.readyRetry) clearTimeout(this.readyRetry); this.readyRetry = null; this.readyDeadline = 0;
    if (this.expiry) clearTimeout(this.expiry); this.expiry = null;
    const changed = this.values.size > 0; this.values.clear();
    if (changed) this.publish();
    if (closed) this.listeners.clear();
  }
  _snapshot(snapshot: ActivitySnapshot): void {
    if (!this.pending || snapshot.snapshotId !== this.requestId) return;
    try {
      const version = uint64(snapshot.version, "INVALID_UPDATE");
      canonicalId(snapshot.snapshotId);
      if (!Number.isInteger(snapshot.chunkCount) || snapshot.chunkCount < 1 || snapshot.chunkCount > 128
        || !Number.isInteger(snapshot.chunkIndex) || snapshot.chunkIndex < 0 || snapshot.chunkIndex >= snapshot.chunkCount) throw new SyncError("INVALID_UPDATE");
      const bytes = toBinary(ActivitySnapshotSchema, snapshot);
      const assembly: NonNullable<InstanceActivities["assembly"]> = this.assembly ??= { id: snapshot.snapshotId, version, count: snapshot.chunkCount, bytes: 0, chunks: new Map() };
      if (assembly.id !== snapshot.snapshotId || assembly.version !== version || assembly.count !== snapshot.chunkCount) throw new SyncError("INVALID_UPDATE");
      const previous = assembly.chunks.get(snapshot.chunkIndex);
      if (previous) {
        if (previous.bytes.length !== bytes.length || previous.bytes.some((value, index) => value !== bytes[index])) throw new SyncError("INVALID_UPDATE");
        return;
      }
      if (assembly.bytes + bytes.length > MAX_BYTES) throw new SyncError("RESOURCE_LIMIT");
      const values = snapshot.activities.map(entry => this.entry(entry));
      assembly.chunks.set(snapshot.chunkIndex, { bytes, values }); assembly.bytes += bytes.length;
      if (assembly.chunks.size !== assembly.count) return;
      if (version < this.version) throw new SyncError("INVALID_UPDATE");
      const next = new Map<string, Activity>();
      for (let index = 0; index < assembly.count; index++) for (const entry of assembly.chunks.get(index)!.values) {
        const key = identity(entry); if (next.has(key)) throw new SyncError("INVALID_UPDATE"); next.set(key, entry);
      }
      this.validateStore(next);
      let applied = version;
      for (const [revision, change] of [...this.buffered].sort(([a], [b]) => a < b ? -1 : a > b ? 1 : 0)) {
        if (revision <= applied) continue;
        if (change.removed) next.delete(identity(change.activity)); else next.set(identity(change.activity), change.activity);
        this.validateStore(next); applied = revision;
      }
      this.cancelPending(); this.initialized = true; this.values = next; this.version = applied;
      this.expire(); this.publish();
    } catch (error) { this.fail(error); }
  }
  _changed(change: ActivityChanged): void {
    if (!this.requested) return;
    try {
      const version = uint64(change.version, "INVALID_UPDATE");
      if (version <= this.version) return;
      if (!change.activity) throw new SyncError("INVALID_UPDATE");
      const activity = this.entry(change.activity, change.removed);
      if (!this.initialized || this.pending) {
        const bytes = toBinary(ActivityChangedSchema, change).length;
        const previous = this.buffered.get(version);
        if (previous) {
          if (JSON.stringify([previous.activity, previous.removed], (_, value) => typeof value === "bigint" ? String(value) : value)
            !== JSON.stringify([activity, change.removed], (_, value) => typeof value === "bigint" ? String(value) : value)) throw new SyncError("INVALID_UPDATE");
          return;
        }
        if (this.buffered.size >= MAX_ENTRIES || this.bufferedBytes + bytes > MAX_BYTES) throw new SyncError("RESOURCE_LIMIT");
        this.buffered.set(version, { activity, removed: change.removed, bytes }); this.bufferedBytes += bytes; return;
      }
      const next = new Map(this.values);
      if (change.removed) next.delete(identity(activity)); else next.set(identity(activity), activity);
      this.validateStore(next); this.values = next; this.version = version; this.expire(); this.publish();
    } catch (error) { this.fail(error); }
  }
  private entry(entry: ActivityEntry, removed = false): Activity {
    const expiry = entry.expiresAtUnixMs;
    if (typeof expiry !== "bigint" || expiry < 0n || expiry > BigInt(Number.MAX_SAFE_INTEGER) || (!removed && expiry === 0n)) throw new SyncError("INVALID_UPDATE");
    return Object.freeze({ presenceId: canonicalId(entry.presenceId), userId: canonicalId(entry.userId), key: activityKey(entry.key), data: activityData(entry.data ?? {}), expiresAtUnixMs: expiry });
  }
  private validateStore(values: Map<string, Activity>): void {
    if (values.size > MAX_ENTRIES) throw new SyncError("RESOURCE_LIMIT");
    const counts = new Map<string, number>();
    for (const entry of values.values()) {
      const count = (counts.get(entry.presenceId) ?? 0) + 1;
      if (count > 16) throw new SyncError("RESOURCE_LIMIT"); counts.set(entry.presenceId, count);
    }
  }
  private cancelPending(): void {
    if (this.deadline) clearTimeout(this.deadline); this.deadline = null;
    this.pending = false; this.requestId = null; this.assembly = null; this.buffered.clear(); this.bufferedBytes = 0;
  }
  private fail(error: unknown): void {
    this.cancelPending();
    this.transport.error(error instanceof SyncError ? error : new SyncError("INVALID_UPDATE"));
  }
  private expire(): void {
    if (this.expiry) clearTimeout(this.expiry); this.expiry = null;
    const now = this.transport.now(); let nearest = Infinity;
    for (const [key, entry] of this.values) {
      const deadline = Number(entry.expiresAtUnixMs);
      if (deadline <= now) this.values.delete(key); else nearest = Math.min(nearest, deadline);
    }
    if (nearest !== Infinity) {
      this.expiry = setTimeout(() => { this.expiry = null; this.expire(); this.publish(); }, Math.max(1, Math.min(30_000, nearest - now)));
      unref(this.expiry);
    }
  }
  private publish(): void {
    this.view = Object.freeze([...this.values.values()].sort((a, b) => identity(a).localeCompare(identity(b))));
    for (const listener of this.listeners) {
      try { listener(this.view); } catch { this.transport.error(new SyncError("HANDLER_FAILED")); }
    }
  }
}
