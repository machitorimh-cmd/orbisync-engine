import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { SnapshotAssembler, SnapshotAssemblyError, type SnapshotAssemblyErrorCode } from "./snapshot_assembler.js";

function frame(index: number, text: string, overrides: Partial<Parameters<SnapshotAssembler["accept"]>[0]["chunk"]> = {}) {
  return {
    generation: 7, instanceId: "room",
    chunk: { snapshotId: "snap", chunkIndex: index, chunkCount: 3, instanceRevision: 9007199254740993n,
      data: new TextEncoder().encode(text), ...overrides },
  };
}

function rejects(assembler: SnapshotAssembler, code: SnapshotAssemblyErrorCode, action: () => unknown): void {
  assert.throws(action, (error: unknown) => error instanceof SnapshotAssemblyError && error.code === code);
  assert.deepEqual(assembler.usage, { bytes: 0, chunks: 0, snapshots: 0 });
  assert.equal(assembler.nextDeadline, undefined);
  assert.throws(() => assembler.accept(frame(0, "x"), 1), { code: "CLOSED" });
}

describe("bounded pure snapshot assembly", () => {
  it("returns bytes only after all unordered indices arrive and retains exact uint64 metadata", () => {
    const assembler = new SnapshotAssembler("room", 7);
    assert.deepEqual(assembler.accept(frame(2, "c"), 0), { status: "pending" });
    assert.deepEqual(assembler.accept(frame(0, "a"), 1), { status: "pending" });
    assert.deepEqual(assembler.accept(frame(2, "c"), 2), { status: "duplicate" });
    assert.deepEqual(assembler.usage, { bytes: 2, chunks: 3, snapshots: 1 });
    const result = assembler.accept(frame(1, "b"), 3);
    assert.equal(result.status, "complete");
    if (result.status !== "complete") assert.fail();
    assert.equal(new TextDecoder().decode(result.snapshot.data), "abc");
    assert.equal(result.snapshot.instanceRevision, 9007199254740993n);
    assert.deepEqual(assembler.usage, { bytes: 0, chunks: 0, snapshots: 0 });
  });

  it("joins raw byte boundaries without parsing or rounding JSON revision", () => {
    const assembler = new SnapshotAssembler("room", 7);
    const bytes = new TextEncoder().encode('{"revision":18446744073709551615,"name":"日本"}');
    const split = bytes.indexOf(0xe6) + 1; // Split inside a UTF-8 code point.
    assembler.accept(frame(1, "", { chunkCount: 2, data: bytes.slice(split) }), 0);
    const result = assembler.accept(frame(0, "", { chunkCount: 2, data: bytes.slice(0, split) }), 1);
    if (result.status !== "complete") assert.fail();
    assert.deepEqual(result.snapshot.data, bytes);
    assert.equal(new TextDecoder("utf-8", { fatal: true }).decode(result.snapshot.data),
      '{"revision":18446744073709551615,"name":"日本"}');
  });

  it("does not mix distinct snapshot IDs at the same revision", () => {
    const assembler = new SnapshotAssembler("room", 7);
    assembler.accept(frame(0, "a", { chunkCount: 2 }), 0);
    assembler.accept(frame(1, "y", { snapshotId: "other", chunkCount: 2 }), 1);
    const first = assembler.accept(frame(1, "b", { chunkCount: 2 }), 2);
    const second = assembler.accept(frame(0, "x", { snapshotId: "other", chunkCount: 2 }), 3);
    if (first.status !== "complete" || second.status !== "complete") assert.fail();
    assert.equal(new TextDecoder().decode(first.snapshot.data), "ab");
    assert.equal(new TextDecoder().decode(second.snapshot.data), "xy");
  });

  it("copies retained bytes and does not expose internal usage counters", () => {
    const assembler = new SnapshotAssembler("room", 7);
    const input = frame(0, "a", { chunkCount: 2 });
    assembler.accept(input, 0);
    input.chunk.data[0] = 120;
    const usage = assembler.usage as { bytes: number };
    usage.bytes = 999;
    assert.equal(assembler.usage.bytes, 1);
    const result = assembler.accept(frame(1, "b", { chunkCount: 2 }), 1);
    if (result.status !== "complete") assert.fail();
    assert.equal(new TextDecoder().decode(result.snapshot.data), "ab");
  });

  it("rejects contradictory duplicates and clears every pending snapshot", () => {
    const assembler = new SnapshotAssembler("room", 7);
    assembler.accept(frame(0, "a"), 0);
    assembler.accept(frame(0, "b", { snapshotId: "other" }), 1);
    rejects(assembler, "CONFLICTING_CHUNK", () => assembler.accept(frame(0, "x"), 2));
  });

  for (const override of [{ chunkCount: 2 }, { instanceRevision: 9007199254740994n }]) {
    it(`rejects changed metadata ${Object.keys(override)[0]}`, () => {
      const assembler = new SnapshotAssembler("room", 7);
      assembler.accept(frame(0, "a"), 0);
      rejects(assembler, "METADATA_MISMATCH", () => assembler.accept(frame(1, "b", override), 1));
    });
  }

  for (const override of [{ chunkCount: 0 }, { chunkCount: -1 }, { chunkCount: 1.5 },
    { chunkIndex: -1 }, { chunkIndex: 3 }, { chunkIndex: 0.5 }, { snapshotId: "" },
    { instanceRevision: -1n }, { instanceRevision: 0x1_0000_0000_0000_0000n }]) {
    it(`rejects invalid metadata ${Object.keys(override)[0]}=${String(Object.values(override)[0])}`, () => {
      const assembler = new SnapshotAssembler("room", 7);
      rejects(assembler, "INVALID_METADATA", () => assembler.accept(frame(0, "a", override), 0));
    });
  }

  it("caps aggregate bytes across IDs without counting duplicates twice", () => {
    const assembler = new SnapshotAssembler("room", 7, { maxBytes: 3 });
    assembler.accept(frame(0, "ab"), 0);
    assembler.accept(frame(0, "ab"), 1);
    rejects(assembler, "RESOURCE_LIMIT", () => assembler.accept(frame(0, "cd", { snapshotId: "other" }), 2));
  });

  it("caps advertised chunks in aggregate, including empty chunks", () => {
    const assembler = new SnapshotAssembler("room", 7, { maxChunks: 5 });
    assembler.accept(frame(0, ""), 0);
    rejects(assembler, "RESOURCE_LIMIT", () => assembler.accept(frame(0, "", { snapshotId: "other" }), 1));
  });

  it("caps pending snapshot IDs before allocating another entry", () => {
    const assembler = new SnapshotAssembler("room", 7, { maxSnapshots: 1 });
    assembler.accept(frame(0, "a"), 0);
    rejects(assembler, "RESOURCE_LIMIT", () => assembler.accept(frame(0, "b", { snapshotId: "other" }), 1));
  });

  it("uses a fixed deadline that duplicates cannot prolong", () => {
    const assembler = new SnapshotAssembler("room", 7, { timeoutMs: 10 });
    assembler.accept(frame(0, "a"), 4);
    assembler.accept(frame(0, "a"), 13);
    assert.equal(assembler.nextDeadline, 14);
    rejects(assembler, "SNAPSHOT_TIMEOUT", () => assembler.expire(14));
  });

  it("rejects a late final chunk rather than completing an expired snapshot", () => {
    const assembler = new SnapshotAssembler("room", 7, { timeoutMs: 10 });
    assembler.accept(frame(0, "a", { chunkCount: 2 }), 0);
    rejects(assembler, "SNAPSHOT_TIMEOUT", () => assembler.accept(frame(1, "b", { chunkCount: 2 }), 10));
  });

  it("ignores old generations even when their metadata or clock would invalidate current data", () => {
    const assembler = new SnapshotAssembler("room", 7);
    assembler.accept(frame(0, "a"), 0);
    assert.deepEqual(assembler.accept({ ...frame(1, "b", { chunkCount: 0 }), generation: 6 }, 100_000),
      { status: "stale-generation" });
    assert.equal(assembler.usage.bytes, 1);
    assert.equal(assembler.nextDeadline, 10_000);
  });

  it("rejects cross-instance frames of the current generation", () => {
    const assembler = new SnapshotAssembler("room", 7);
    rejects(assembler, "INSTANCE_MISMATCH", () => assembler.accept({ ...frame(0, "a"), instanceId: "elsewhere" }, 0));
  });

  it("disposes all retained resources idempotently", () => {
    const assembler = new SnapshotAssembler("room", 7);
    assembler.accept(frame(0, "a"), 0);
    assembler.dispose();
    assembler.dispose();
    assert.deepEqual(assembler.usage, { bytes: 0, chunks: 0, snapshots: 0 });
    assert.equal(assembler.nextDeadline, undefined);
    assert.throws(() => assembler.expire(0), { code: "CLOSED" });
  });

  it("rejects unusable resource configuration", () => {
    for (const limits of [{ maxBytes: 0 }, { maxChunks: NaN }, { timeoutMs: Infinity }, { maxSnapshots: -1 }]) {
      assert.throws(() => new SnapshotAssembler("room", 7, limits), { code: "INVALID_METADATA" });
    }
  });
});
