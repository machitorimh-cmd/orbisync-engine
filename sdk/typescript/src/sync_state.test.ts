import { describe, it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EntityCommandSchema, StateDeltaSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { SyncStore, parseStateJson, uint64 } from "./sync_state.js";

export function snapshotBytes(revision = 100n): Uint8Array {
  return new TextEncoder().encode(`{"format":"orbisync.snapshot.v1","revision":${revision},"instance":{"instance_id":"room"},"entities":[],"users":[]}`);
}
function command(operation: string, boundary: bigint, revision: bigint, args = {}) {
  return create(EntityCommandSchema, { entityId: "a", operation, instanceRevision: boundary, expectedRevision: revision, arguments: args });
}
const update = (boundary: bigint, revision: bigint, key: string, value: string) => command("update", boundary, revision, { component_key: key, value });

describe("lossless canonical state", () => {
  it("combines distinct fields at one boundary and retains a delayed different entity", () => {
    for (const reverse of [false, true]) {
      const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
      const delta = create(StateDeltaSchema, { fromRevision: 100n, toRevision: 101n,
        entities: [{ entityId: "a", revision: 2n, transform: { positionX: 1, rotationW: 1 } }] });
      const apply = [() => store.applyCommand(update(101n, 2n, "note.a", "A")), () => store.applyDelta(delta)];
      for (const action of reverse ? apply.reverse() : apply) action();
      store.applyCommand(update(105n, 3n, "note.a", "new A"));
      store.applyDelta(create(StateDeltaSchema, { fromRevision: 101n, toRevision: 102n,
        entities: [{ entityId: "b", revision: 7n, properties: { "note.b": { encoding: "json", value: "B" } } }] }));
      const state = store.state;
      assert.equal(state.entities.get("a")?.transform?.positionX, 1);
      assert.equal(state.entities.get("a")?.revision, 3n);
      assert.equal(state.entities.get("b")?.revision, 7n);
      assert.deepEqual(state.entities.get("b")?.properties["note.b"], { encoding: "json", value: "B" });
    }
  });
  it("retains a full-properties floor even when the newer map is empty", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
    store.applyDelta(create(StateDeltaSchema, { fromRevision: 102n, toRevision: 103n,
      entities: [{ entityId: "a", revision: 3n, properties: {} }] }));
    store.applyCommand(update(102n, 2n, "note.old", "must not resurrect"));
    store.applyDelta(create(StateDeltaSchema, { fromRevision: 100n, toRevision: 101n,
      entities: [{ entityId: "a", revision: 1n, properties: { "note.older": { encoding: "json", value: "old" } } }] }));
    assert.deepEqual(Object.keys(store.state.entities.get("a")!.properties), []);
    store.applyCommand(update(104n, 4n, "note.new", "kept"));
    assert.deepEqual(Object.keys(store.state.entities.get("a")!.properties), ["note.new"]);
  });
  it("bounds aggregate retained values and rolls back an oversized staged update", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room", 4096);
    store.applyCommand(update(101n, 1n, "note.a", "A"));
    const staged = store.fork();
    assert.throws(() => staged.applyCommand(update(102n, 2n, "note.b", "B".repeat(1800))), { code: "RESOURCE_LIMIT" });
    assert.deepEqual(Object.keys(store.state.entities.get("a")!.properties), ["note.a"]);
  });
  it("retains exact JSON integers before uint64 validation", () => {
    for (const revision of [9007199254740993n, 18446744073709551615n]) {
      assert.equal(SyncStore.snapshot(snapshotBytes(revision), revision, "room").revision, revision);
      assert.equal(parseStateJson(revision.toString()), revision);
    }
    assert.throws(() => uint64(9007199254740992), { code: "INVALID_SNAPSHOT" });
    assert.throws(() => uint64(18446744073709551616n), { code: "INVALID_SNAPSHOT" });
    assert.throws(() => parseStateJson('{"revision":1,"revision":2}'), { code: "INVALID_SNAPSHOT" });
    assert.throws(() => parseStateJson('9007199254740993.0'), { code: "INVALID_SNAPSHOT" });
  });
  it("merges reversed independent components without regressing entity revision", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
    store.applyCommand(update(102n, 7n, "note.b", "B"));
    store.applyDelta(create(StateDeltaSchema, { fromRevision: 100n, toRevision: 101n, entities: [{ entityId: "a", revision: 6n, properties: { "note.a": { encoding: "json", value: "A" } } }] }));
    const entity = store.state.entities.get("a")!;
    assert.equal(entity.revision, 7n);
    assert.deepEqual(Object.keys(entity.properties).sort(), ["note.a", "note.b"]);
  });
  it("accepts update before respawn and ignores old incarnation delta and late delete", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
    store.applyCommand(update(102n, 9n, "note.old", "old"));
    store.applyCommand(update(105n, 2n, "note.new", "new"));
    store.applyCommand(command("spawn", 104n, 1n, { kind: "NOTE", text: "echo only" }));
    store.applyCommand(command("delete", 103n, 103n));
    store.applyDelta(create(StateDeltaSchema, { fromRevision: 101n, toRevision: 102n, entities: [{ entityId: "a", revision: 9n, properties: { "note.old": { encoding: "json", value: "old" } } }] }));
    const entity = store.state.entities.get("a")!;
    assert.equal(entity.kind, "note");
    assert.equal(entity.revision, 2n);
    assert.deepEqual(Object.keys(entity.properties), ["note.new"]);
  });
  it("never persists spawn echo and resets entity revision on respawn", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
    store.applyCommand(update(101n, 9n, "note.body", "old"));
    store.applyCommand(command("delete", 102n, 102n));
    store.applyCommand(command("spawn", 103n, 1n, { kind: "note", text: "not persisted", color: "red" }));
    assert.equal(store.entityRevision("a"), 1n);
    assert.deepEqual(Object.keys(store.state.entities.get("a")!.properties), []);
  });
  it("isolates staged failures and detached public copies", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
    store.applyCommand(update(101n, 1n, "note.a", "A"));
    const staged = store.fork();
    assert.throws(() => staged.applyCommand(update(101n, 1n, "note.a", "different")), { code: "REVISION_CONFLICT" });
    const copy = store.state.entities.get("a")!;
    copy.properties["note.a"] = null;
    assert.notEqual(store.state.entities.get("a")!.properties["note.a"], null);
    assert.equal(store.revision, 101n);
  });
  it("rejects unsupported confirmations instead of inventing a boundary", () => {
    const store = SyncStore.snapshot(snapshotBytes(), 100n, "room");
    assert.throws(() => store.applyCommand(create(EntityCommandSchema, { entityId: "a", operation: "spawn", expectedRevision: 1n, arguments: { kind: "note" } })), { code: "UNSUPPORTED_SERVER" });
  });
});
