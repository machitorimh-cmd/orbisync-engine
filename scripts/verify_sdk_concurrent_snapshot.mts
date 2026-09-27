/** Actual production PG + ticket + TCP WebSocket snapshot/backpressure test. */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { WebSocket as NodeWebSocket } from "../sdk/typescript/node_modules/ws/wrapper.mjs";
import { OrbiSyncClient, uuidv7, decodeEnvelope, type OrbiSyncInstance } from "../sdk/typescript/src/client.js";

const [base, container, db, worldId] = process.argv.slice(2);
globalThis.WebSocket = NodeWebSocket as unknown as typeof WebSocket;
if (!base || !container || !db || !/^[-a-f0-9]{36}$/.test(worldId ?? "")) throw Error("usage: verify_sdk_concurrent_snapshot.mts <isolated Core> <isolated container> <DB> <world>");
const writer = new OrbiSyncClient({ baseUrl: base });
await writer.auth.guest();
const owner = JSON.parse(Buffer.from(writer._getAccessToken()!.split(".")[1]!, "base64url").toString()).sub;
assert.match(owner, /^[-a-f0-9]{36}$/);
const instanceId = uuidv7(), targets = [uuidv7(), uuidv7()];
const fillers = Array.from({ length: 500 }, () => uuidv7());
// Only new IDs, before actor activation. No existing demo rows are modified.
const rows = [...targets, ...fillers].map(id => `('${id}','${instanceId}','object','${owner}','{"type":"global"}',1,now(),now())`).join(",");
const sql = `BEGIN;
INSERT INTO world_instances(id,world_id,lifecycle,capacity,created_at,started_at,revision) VALUES ('${instanceId}','${worldId}','running',100,now(),now(),1000);
INSERT INTO persistent_entities(id,instance_id,kind,owner_id,visibility,revision,created_at,updated_at) VALUES ${rows};
INSERT INTO persistent_entity_components(entity_id,component_key,payload)
SELECT id,'test.fill',convert_to(json_build_object('text',repeat('x',3000))::text,'UTF8') FROM persistent_entities WHERE instance_id='${instanceId}';
COMMIT;`;
execFileSync("docker", ["exec", "-i", container, "psql", "-U", db, "-d", db, "-v", "ON_ERROR_STOP=1"], { input: sql, stdio: ["pipe", "pipe", "pipe"] });
const connections: Awaited<ReturnType<OrbiSyncClient["connect"]>>[] = [];
const delay = (ms: number) => new Promise(resolve => setTimeout(resolve, ms));
async function confirmed(instance: OrbiSyncInstance, entityId: string, step: number): Promise<void> {
  const previous = instance.state.entities.get(entityId)!.revision;
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => done(Error("transform confirmation timeout")), 10000);
    const ack = () => { if (instance.state.entities.get(entityId)!.revision > previous) done(); };
    const error = (raw: unknown) => done(Error((raw as { code?: string }).code));
    const done = (failure?: Error) => { clearTimeout(timer); instance.off("entityUpdated", ack); instance.off("error", error); if (failure) reject(failure); else resolve(); };
    instance.on("entityUpdated", ack); instance.on("error", error);
    try { instance.sendTransform({ entityId, expectedRevision: previous, position: { x: step * 0.001, y: 0, z: 0 } }); } catch (error) { done(error as Error); }
  });
}
try {
  const first = await writer.connect(); connections.push(first);
  const a = await first.join(instanceId); await a.ready();
  const reader = new OrbiSyncClient({ baseUrl: base }); await reader.auth.nameOnly({ displayName: "snapshot concurrent reader" });
  const second = await reader.connect(); connections.push(second);
  const transport = second._getWs() as unknown as WebSocket & { _socket: { pause(): void; resume(): void } };
  assert.ok(transport._socket, "real Node TCP WebSocket required for backpressure");
  let chunks = 0, advertised = 0, completed = false, commitsDuringChunks = 0, writing = true;
  let writes: Promise<void> | undefined;
  const snapshotIds = new Set<string>();
  transport.addEventListener("message", event => {
    const envelope = decodeEnvelope(new Uint8Array(event.data as ArrayBuffer));
    if (envelope.payload.case !== "snapshot") return;
    const chunk = envelope.payload.value;
    chunks++; advertised = chunk.chunkCount; snapshotIds.add(chunk.snapshotId);
    if (chunks === 1) {
      // Pause the actual TCP receive stream, not SDK events or a mocked queue.
      transport._socket.pause();
      setTimeout(() => transport._socket.resume(), 600);
      writes = (async () => {
        try {
          for (let i = 0; i < 12; i++) {
            await confirmed(a, targets[i % targets.length]!, i + 1);
            if (!completed) commitsDuringChunks++;
            await delay(150);
          }
        } finally { writing = false; }
      })();
      // Attach immediately so a failed writer cannot become an unhandled rejection.
      void writes.catch(() => {});
    }
    if (chunks === advertised) completed = true;
  });
  const b = await second.join(instanceId); await b.ready();
  const readyWhileWriting = writing;
  assert.ok(writes); await writes;
  const until = Date.now() + 10000;
  while (targets.some(id => b.state.entities.get(id)?.revision !== a.state.entities.get(id)?.revision)) {
    if (Date.now() > until) throw Error("post-snapshot convergence timeout");
    await delay(20);
  }
  for (const id of targets) {
    assert.deepEqual(b.state.entities.get(id)?.properties, a.state.entities.get(id)?.properties);
    assert.deepEqual(b.state.entities.get(id)?.transform, a.state.entities.get(id)?.transform);
  }
  assert.ok(chunks > 80, "must exercise large production chunk stream");
  assert.ok(commitsDuringChunks > 0, "must confirm real commits between first and last received chunks");
  assert.equal(snapshotIds.size, 1, "ordinary concurrent updates must not require a fresh snapshot");
  assert.equal(b.syncStatus, "ready");
  assert.equal(readyWhileWriting, true, "readiness must progress while the world keeps changing");
  console.log(JSON.stringify({ pass: true, instanceId, seeded: fillers.length + targets.length, actualSnapshotChunks: chunks, commitsDuringChunks, readyWhileWriting, snapshotGenerations: snapshotIds.size, convergedEntities: targets.length }));
} finally { await Promise.all(connections.map(connection => connection.disconnect())); }


