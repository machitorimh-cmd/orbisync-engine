import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { EntityCommandSchema, StateDeltaSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { SyncStore } from "./sync_state.js";

function store(limit?: number) {
  return SyncStore.snapshot(new TextEncoder().encode(
    '{"format":"orbisync.snapshot.v1","revision":0,"instance":{"instance_id":"room"},"entities":[],"users":[]}',
  ), 0n, "room", limit);
}
function delta(entityId: string, boundary: bigint, x = 1) {
  return create(StateDeltaSchema, { fromRevision: boundary - 1n, toRevision: boundary,
    entities: [{ entityId, revision: boundary, transform: { positionX: x, rotationW: 1 } }] });
}
function command(entityId: string, operation: string, boundary: bigint, args = {}) {
  return Object.assign(create(EntityCommandSchema, {
    entityId, operation, expectedRevision: boundary, arguments: args,
  }), { instanceRevision: boundary });
}

it("atomic changes roll back updates, insertions, tombstones and budget together", () => {
  const state = store(4096);
  state.transaction(s => s.applyDelta(delta("a", 1n)));
  const before = state.state;
  assert.throws(() => state.transaction(s => {
    s.applyCommand(command("a", "delete", 2n));
    s.applyDelta(delta("b", 3n));
    s.applyCommand(command("b", "update", 4n, { component_key: "note.text", value: "x".repeat(4096) }));
  }), { code: "RESOURCE_LIMIT" });
  assert.deepEqual(state.state, before);
  state.transaction(s => s.applyDelta(delta("a", 2n, 2)));
  assert.equal(state.entityRevision("a"), 2n, "failed deletion must not leave a tombstone");
  assert.equal(state.entityRevision("b"), undefined);
  assert.throws(() => state.transaction(s => {
    s.applyDelta(delta("b", 3n));
    s.applyDelta(delta("a", 2n, 99));
  }), { code: "REVISION_CONFLICT" });
  assert.equal(state.entityRevision("b"), undefined);
  assert.equal(state.revision, 2n);
});

it("atomic changes preserve deletion, reinsertion order and detached public values", () => {
  const state = store();
  state.transaction(s => {
    s.applyDelta(delta("a", 1n));
    s.applyDelta(delta("b", 2n));
  });
  const oldPublic = state.state;
  state.transaction(s => {
    s.applyCommand(command("a", "delete", 3n));
    s.applyCommand(command("a", "spawn", 4n, { kind: "note" }));
    s.applyDelta(delta("temporary", 5n));
    s.applyCommand(command("temporary", "delete", 6n));
  });
  assert.deepEqual([...state.state.entities.keys()], ["b", "a"]);
  state.transaction(s => s.applyDelta(delta("a", 2n, 99)));
  assert.equal(state.entityRevision("a"), 4n);
  oldPublic.entities.get("a")!.transform!.positionX = 99;
  const exposed = state.state.entities.get("a")!;
  exposed.properties["note.external"] = null;
  assert.equal(state.state.entities.get("a")!.properties["note.external"], undefined);
  assert.equal(oldPublic.entities.get("a")!.revision, 1n);
});

it("atomic single-entity updates do not iterate the complete entity index", () => {
  const state = store();
  state.transaction(s => {
    for (let i = 1; i <= 12; i++) s.applyDelta(delta(String(i), BigInt(i)));
  });
  const index = (state as unknown as { entities: Map<string, unknown> }).entities;
  const iterator = index[Symbol.iterator];
  index[Symbol.iterator] = () => { throw new Error("full index traversal"); };
  try {
    state.transaction(s => s.applyDelta(delta("1", 13n, 2)));
    assert.equal(state.entityRevision("1"), 13n);
    assert.equal(state.entityRevision("12"), 12n);
  } finally { index[Symbol.iterator] = iterator; }
});
