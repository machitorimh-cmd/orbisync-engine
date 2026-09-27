import type { Snapshot } from "./generated/orbisync/v1/realtime_pb.js";

export type SnapshotAssemblyErrorCode =
  | "INVALID_METADATA" | "INSTANCE_MISMATCH" | "METADATA_MISMATCH"
  | "CONFLICTING_CHUNK" | "RESOURCE_LIMIT" | "SNAPSHOT_TIMEOUT" | "CLOSED";

/** Contains a stable code, never untrusted payload bytes or credentials. */
export class SnapshotAssemblyError extends Error {
  constructor(readonly code: SnapshotAssemblyErrorCode) {
    super(`Snapshot assembly failed: ${code}`);
    this.name = "SnapshotAssemblyError";
  }
}

export type SnapshotAssemblyLimits = {
  maxBytes: number;
  maxChunks: number;
  maxSnapshots: number;
  timeoutMs: number;
};

export const DEFAULT_SNAPSHOT_ASSEMBLY_LIMITS: Readonly<SnapshotAssemblyLimits> = Object.freeze({
  maxBytes: 16 * 1024 * 1024,
  maxChunks: 1024,
  maxSnapshots: 2,
  timeoutMs: 10_000,
});

type SnapshotChunk = Pick<Snapshot,
  "snapshotId" | "chunkIndex" | "chunkCount" | "instanceRevision" | "data">;

export type AssembledSnapshot = {
  snapshotId: string;
  instanceId: string;
  generation: number;
  instanceRevision: bigint;
  data: Uint8Array;
  chunkLengths: number[];
};

export type SnapshotAssemblyResult =
  | { status: "stale-generation" | "pending" | "duplicate" }
  | { status: "complete"; snapshot: AssembledSnapshot };

type PendingSnapshot = {
  revision: bigint;
  count: number;
  deadline: number;
  chunks: Map<number, Uint8Array>;
  bytes: number;
};

/**
 * Byte assembly only: no JSON parsing, state reduction, recovery, or event dispatch.
 * One owner per connection generation/instance. Its owner must arm one timer for
 * nextDeadline and call expire(), and dispose on abort/disconnect/leave. Passing
 * time explicitly makes expiration deterministic and leaves no internal timers.
 * Completion is NOT readiness: UTF-8/JSON/schema/revision validation must follow.
 * Completed IDs are not retained here; publication order and completed-snapshot
 * dedup belong to the state owner. Joining needs at most maxBytes extra scratch
 * space, in addition to the bounded retained chunks and caller-owned input.
 */
export class SnapshotAssembler {
  private readonly limits: Readonly<SnapshotAssemblyLimits>;
  private pending = new Map<string, PendingSnapshot>();
  private bytes = 0;
  private reservedChunks = 0;
  private closed = false;

  constructor(
    private readonly instanceId: string,
    private readonly generation: number,
    limits: Partial<SnapshotAssemblyLimits> = {},
  ) {
    this.limits = Object.freeze({ ...DEFAULT_SNAPSHOT_ASSEMBLY_LIMITS, ...limits });
    if (!instanceId || instanceId.length > 256 || !Number.isSafeInteger(generation) || generation < 0
      || Object.values(this.limits).some(value => !Number.isSafeInteger(value) || value <= 0)) {
      throw new SnapshotAssemblyError("INVALID_METADATA");
    }
  }

  /** Detached counters; maxChunks reserves the advertised counts, across all IDs. */
  get usage(): Readonly<{ bytes: number; chunks: number; snapshots: number }> {
    return { bytes: this.bytes, chunks: this.reservedChunks, snapshots: this.pending.size };
  }

  get nextDeadline(): number | undefined {
    let deadline: number | undefined;
    for (const entry of this.pending.values()) {
      deadline = deadline === undefined ? entry.deadline : Math.min(deadline, entry.deadline);
    }
    return deadline;
  }

  /** An expired incomplete snapshot invalidates the whole assembly attempt. */
  expire(nowMs: number): void {
    this.ensureOpen();
    this.validateTime(nowMs);
    const deadline = this.nextDeadline;
    if (deadline !== undefined && nowMs >= deadline) this.fail("SNAPSHOT_TIMEOUT");
  }

  accept(input: {
    generation: number;
    instanceId: string;
    chunk: SnapshotChunk;
  }, nowMs: number): SnapshotAssemblyResult {
    this.ensureOpen();
    // Late old-socket events cannot expire or corrupt the new generation.
    if (input.generation !== this.generation) return { status: "stale-generation" };
    this.expire(nowMs);
    if (input.instanceId !== this.instanceId) this.fail("INSTANCE_MISMATCH");
    const chunk = input.chunk;
    if (typeof chunk.snapshotId !== "string" || !chunk.snapshotId || chunk.snapshotId.length > 256
      || !Number.isInteger(chunk.chunkCount) || chunk.chunkCount <= 0 || chunk.chunkCount > 0xffff_ffff
      || !Number.isInteger(chunk.chunkIndex) || chunk.chunkIndex < 0 || chunk.chunkIndex >= chunk.chunkCount
      || typeof chunk.instanceRevision !== "bigint" || chunk.instanceRevision < 0n
      || chunk.instanceRevision > 0xffff_ffff_ffff_ffffn || !(chunk.data instanceof Uint8Array)) {
      this.fail("INVALID_METADATA");
    }

    let entry = this.pending.get(chunk.snapshotId);
    if (entry) {
      if (entry.revision !== chunk.instanceRevision || entry.count !== chunk.chunkCount) {
        this.fail("METADATA_MISMATCH");
      }
      const previous = entry.chunks.get(chunk.chunkIndex);
      if (previous !== undefined) {
        if (previous.length !== chunk.data.length || previous.some((byte, index) => byte !== chunk.data[index])) {
          this.fail("CONFLICTING_CHUNK");
        }
        return { status: "duplicate" };
      }
    } else {
      if (this.pending.size >= this.limits.maxSnapshots
        || chunk.chunkCount > this.limits.maxChunks - this.reservedChunks) this.fail("RESOURCE_LIMIT");
      const deadline = nowMs + this.limits.timeoutMs;
      if (!Number.isSafeInteger(deadline)) this.fail("INVALID_METADATA");
      entry = { revision: chunk.instanceRevision, count: chunk.chunkCount, deadline, chunks: new Map(), bytes: 0 };
      this.pending.set(chunk.snapshotId, entry);
      this.reservedChunks += chunk.chunkCount;
    }

    if (chunk.data.byteLength > this.limits.maxBytes - this.bytes) this.fail("RESOURCE_LIMIT");
    // Copy before retention: the transport/caller must not mutate pending data.
    entry.chunks.set(chunk.chunkIndex, new Uint8Array(chunk.data));
    entry.bytes += chunk.data.byteLength;
    this.bytes += chunk.data.byteLength;
    if (entry.chunks.size !== entry.count) return { status: "pending" };

    const data = new Uint8Array(entry.bytes);
    let offset = 0;
    for (let index = 0; index < entry.count; index++) {
      const part = entry.chunks.get(index)!;
      data.set(part, offset);
      offset += part.length;
    }
    this.pending.delete(chunk.snapshotId);
    this.bytes -= entry.bytes;
    this.reservedChunks -= entry.count;
    return {
      status: "complete",
      snapshot: {
        snapshotId: chunk.snapshotId, instanceId: this.instanceId, generation: this.generation,
        instanceRevision: entry.revision, data,
        chunkLengths: Array.from({ length: entry.count }, (_, index) => entry.chunks.get(index)!.length),
      },
    };
  }

  dispose(): void {
    this.pending.clear();
    this.bytes = 0;
    this.reservedChunks = 0;
    this.closed = true;
  }

  /** A newer complete baseline supersedes older incomplete snapshots. */
  discardOlderThan(revision: bigint): void {
    this.ensureOpen();
    for (const [id, entry] of this.pending) {
      if (entry.revision < revision) {
        this.pending.delete(id);
        this.bytes -= entry.bytes;
        this.reservedChunks -= entry.count;
      }
    }
  }

  private ensureOpen(): void {
    if (this.closed) throw new SnapshotAssemblyError("CLOSED");
  }

  private validateTime(nowMs: number): void {
    if (!Number.isSafeInteger(nowMs) || nowMs < 0) this.fail("INVALID_METADATA");
  }

  private fail(code: SnapshotAssemblyErrorCode): never {
    this.dispose();
    throw new SnapshotAssemblyError(code);
  }
}
