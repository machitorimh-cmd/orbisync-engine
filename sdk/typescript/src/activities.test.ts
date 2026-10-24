import { it } from "node:test";
import assert from "node:assert/strict";
import { create } from "@bufbuild/protobuf";
import { InstanceActivities } from "./activities.js";
import { ActivityEntrySchema, ActivitySnapshotSchema, ActivityChangedSchema } from "./generated/orbisync/v1/realtime_pb.js";
import { SyncError } from "./sync_state.js";
const presence = "0192d43d-a18a-7fed-8123-0123456789ab";
const user = "0192d43d-a18a-7fed-8123-1123456789ab";
const snapshotId = "0192d43d-a18a-7fed-8123-2123456789ab";
function setup() {
  let now = 10_000, available = true, requests = 0, requestId = "";
  const sent: { key: string; data: unknown; ttlMs: number }[] = [], errors: SyncError[] = [];
  const activities = new InstanceActivities({
    check() { if (!available) throw new SyncError("NOT_READY"); },
    update(key, data, ttlMs) { sent.push({ key, data, ttlMs }); },
    requestSnapshot(id) { requests++; requestId = id; }, ownPresenceId: () => presence, now: () => now,
    error: error => errors.push(error),
  });
  return { activities, sent, errors, requests: () => requests, requestId: () => requestId,
    snapshot(...args: Parameters<typeof snapshot>) { const result = snapshot(...args); result.snapshotId = requestId; return result; },
    setNow(value: number) { now = value; }, disconnect() { available = false; activities._reset(); } };
}
function entry(key = "app.typing", data: Record<string, never> = {}, expiry = 15_000n) {
  return create(ActivityEntrySchema, { presenceId: presence, userId: user, key, data, expiresAtUnixMs: expiry });
}
function snapshot(entries = [entry()], version = 1n, chunkIndex = 0, chunkCount = 1) {
  return create(ActivitySnapshotSchema, { activities: entries, version, snapshotId, chunkIndex, chunkCount });
}
function changed(key = "app.typing", version = 2n, removed = false) {
  return create(ActivityChangedSchema, { activity: entry(key), version, removed });
}
it("activities validate namespace, JSON and TTL, own copies, and send clear without replay", () => {
  const f = setup(); const data = { nested: { label: "hello" } };
  f.activities.set({ key: "app.typing", data }); data.nested.label = "mutated";
  assert.deepEqual(f.sent[0], { key: "app.typing", data: { nested: { label: "hello" } }, ttlMs: 5000 });
  assert.ok(Object.isFrozen((f.sent[0].data as { nested: unknown }).nested));
  for (const key of ["typing", "app..typing", "core.typing", "orbisync.typing", "system.typing", "admin.typing", "App.typing", "app.💡", "app." + "a".repeat(125)]) assert.throws(() => f.activities.set({ key }), { code: "INVALID_UPDATE" });
  for (const ttlMs of [0, 249, 30_001, 250.5, NaN]) assert.throws(() => f.activities.set({ key: "app.typing", ttlMs }), { code: "INVALID_UPDATE" });
  for (const value of [new Uint8Array([1]), new Date(), { value: undefined }, { value: 1n }]) assert.throws(() => f.activities.set({ key: "app.typing", data: value as never }), { code: "INVALID_UPDATE" });
  assert.throws(() => f.activities.set({ key: "app.typing", data: { value: "x".repeat(4096) } }), { code: "RESOURCE_LIMIT" });
  f.activities.set({ key: "app.typing", data: { value: "\u0000".repeat(1000) } }); // JSON escaping exceeds 4 KiB, protobuf Struct does not.
  f.activities.set({ key: "app.typing", ttlMs: 250 }); f.activities.set({ key: "app.typing", ttlMs: 30_000 });
  f.activities.clear("app.typing"); assert.equal(f.sent.at(-1)?.ttlMs, 0);
  f.disconnect(); assert.throws(() => f.activities.set({ key: "app.typing" }), { code: "NOT_READY" }); assert.equal(f.sent.length, 5);
});
it("activities assemble out-of-order chunks atomically and reconcile intervening changes with version gaps", () => {
  const f = setup(), notifications: number[] = [];
  f.activities.onChange(value => notifications.push(value.length)); f.activities._ready(); f.activities._ready();
  assert.equal(f.requests(), 1);
  f.activities._snapshot(f.snapshot([entry("app.a")], 100n, 1, 2));
  f.activities._changed(changed("app.a", 104n, true));
  f.activities._changed(changed("app.c", 110n));
  assert.equal(f.activities.current.length, 0); assert.equal(notifications.length, 0);
  f.activities._snapshot(f.snapshot([entry("app.b")], 100n, 0, 2));
  assert.deepEqual(f.activities.current.map(item => item.key), ["app.b", "app.c"]); assert.deepEqual(notifications, [2]);
  f.activities._changed(changed("app.stale", 109n));
  assert.deepEqual(f.activities.current.map(item => item.key), ["app.b", "app.c"]);
  assert.ok(Object.isFrozen(f.activities.current)); assert.ok(Object.isFrozen(f.activities.current[0].data));
  f.activities._reset(); assert.equal(f.activities.current.length, 0);
  f.activities._changed(changed("app.old_socket", 999n)); assert.equal(f.activities.current.length, 0);
  f.activities._ready(); assert.equal(f.requests(), 2); f.activities._snapshot(f.snapshot([], 500n)); assert.equal(f.errors.length, 0);
  f.activities._reset();
});
it("activity snapshot duplication, conflicting chunks, bounds and per-presence capacity are checked atomically", () => {
  const f = setup(); f.activities._ready();
  f.activities._snapshot(f.snapshot([entry()], 1n, 0, 2));
  f.activities._snapshot(f.snapshot([entry()], 1n, 0, 2)); assert.equal(f.errors.length, 0);
  f.activities._snapshot(f.snapshot([entry("app.other")], 1n, 0, 2)); assert.equal(f.errors.at(-1)?.code, "INVALID_UPDATE"); assert.deepEqual(f.activities.current, []);
  f.activities.refresh(); f.activities._snapshot(f.snapshot([], 1n, 0, 129)); assert.equal(f.errors.at(-1)?.code, "INVALID_UPDATE");
  f.activities.refresh(); f.activities._snapshot(f.snapshot(Array.from({ length: 17 }, (_, index) => entry(`app.key${index}`))));
  assert.equal(f.errors.at(-1)?.code, "RESOURCE_LIMIT"); assert.deepEqual(f.activities.current, []);
  f.activities.refresh(); f.activities._snapshot(f.snapshot([entry(), entry()])); assert.equal(f.errors.at(-1)?.code, "INVALID_UPDATE");
  f.activities.refresh(); f.activities._snapshot(f.snapshot(Array.from({ length: 16 }, (_, index) => entry(`app.key${index}`))));
  assert.equal(f.activities.current.length, 16);
  assert.throws(() => f.activities.set({ key: "app.extra" }), { code: "RESOURCE_LIMIT" });
  f.activities.set({ key: "app.key0" }); assert.equal(f.sent.length, 1);
  f.activities._changed(changed("app.extra", 3n)); assert.equal(f.activities.current.length, 16); assert.equal(f.errors.at(-1)?.code, "RESOURCE_LIMIT");
  f.activities._reset();
});
it("activity assembly has a fixed deadline and bounded total bytes, with no partial publication", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const f = setup(); f.activities._ready(); f.activities._snapshot(f.snapshot([entry()], 1n, 0, 2));
  const oldResponse = f.snapshot([entry()], 1n, 1, 2);
  t.mock.timers.tick(9999); f.activities._snapshot(f.snapshot([entry()], 1n, 0, 2)); t.mock.timers.tick(1);
  assert.equal(f.errors.at(-1)?.code, "SYNC_TIMEOUT"); assert.deepEqual(f.activities.current, []);
  f.activities.refresh();
  assert.notEqual(oldResponse.snapshotId, f.requestId()); f.activities._snapshot(oldResponse);
  assert.equal(f.errors.length, 1); assert.equal(f.activities.current.length, 0);
  for (let index = 0; index < 128 && f.errors.length === 1; index++) {
    const items = Array.from({ length: 16 }, (_, key) => create(ActivityEntrySchema, {
      ...entry(`app.key${key}`, { text: "x".repeat(4000) } as never), presenceId: `0192d43d-a18a-7fed-8123-${index.toString(16).padStart(12, "0")}`,
    }));
    f.activities._snapshot(f.snapshot(items, 2n, index, 128));
  }
  assert.equal(f.errors.at(-1)?.code, "RESOURCE_LIMIT"); assert.deepEqual(f.activities.current, []); f.activities._reset();
});
it("activities expire locally, isolate listener errors and preserve the version floor", t => {
  t.mock.timers.enable({ apis: ["setTimeout"] });
  const f = setup(); f.activities._ready(); f.activities._snapshot(f.snapshot([entry("app.typing", {}, 10_250n)]));
  let changes = 0; const unsubscribe = f.activities.onChange(() => { changes++; });
  f.activities.onChange(() => { throw new Error("app bug"); });
  f.setNow(10_250); t.mock.timers.tick(250);
  assert.deepEqual(f.activities.current, []); assert.equal(changes, 1); assert.equal(f.errors.at(-1)?.code, "HANDLER_FAILED");
  f.activities._changed(changed("app.stale", 1n)); assert.deepEqual(f.activities.current, []);
  unsubscribe(); f.activities._reset(true);
});
