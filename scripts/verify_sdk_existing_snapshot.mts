/** Read-only acceptance against a previously seeded, oversized JoinAccepted fixture. */
import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { OrbiSyncClient, decodeEnvelope } from "../sdk/typescript/src/client.js";
const [base, instanceId, container, db, expectedCount] = process.argv.slice(2);
if (!base || !container || !db || !/^[-a-f0-9]{36}$/.test(instanceId ?? "") || !/^\d+$/.test(expectedCount ?? "")) throw Error("explicit isolated fixture and expected count required");
const rows = JSON.parse(execFileSync("docker", ["exec", container, "psql", "-U", db, "-d", db, "-tAc",
  `SELECT coalesce(json_agg(json_build_object('id',e.id,'revision',e.revision::text,'key',c.component_key,'hex',encode(c.payload,'hex'))),'[]') FROM persistent_entities e LEFT JOIN persistent_entity_components c ON c.entity_id=e.id WHERE e.instance_id='${instanceId}'`], { encoding: "utf8" }).trim());
const expected = new Map<string, { revision: bigint; properties: Record<string, unknown> }>();
for (const row of rows) {
  if (!expected.has(row.id)) expected.set(row.id, { revision: BigInt(row.revision), properties: {} });
  if (row.key && !row.key.startsWith("core.")) expected.get(row.id)!.properties[row.key] = { encoding: "json", value: JSON.parse(Buffer.from(row.hex, "hex").toString("utf8")) };
}
assert.equal(expected.size, Number(expectedCount));
const client = new OrbiSyncClient({ baseUrl: base }); await client.auth.guest();
const connection = await client.connect();
let chunks = 0, maxFrame = 0, maxNormalFrame = 0, joinStateCount = -1;
const snapshots = new Set<string>();
connection._getWs().addEventListener("message", event => {
  const frame = new Uint8Array(event.data as ArrayBuffer), envelope = decodeEnvelope(frame);
  maxFrame = Math.max(maxFrame, frame.length);
  if (envelope.payload.case === "snapshot") { chunks++; snapshots.add(envelope.payload.value.snapshotId); assert.ok(envelope.payload.value.data.length <= 16384); }
  else maxNormalFrame = Math.max(maxNormalFrame, frame.length);
  if (envelope.payload.case === "joinAccepted") joinStateCount = envelope.payload.value.nearbyEntities.length;
});
try {
  const instance = await connection.join(instanceId); await instance.ready();
  const state = instance.state;
  assert.equal(state.entities.size, expected.size);
  for (const [id, row] of expected) {
    assert.equal(state.entities.get(id)?.revision, row.revision);
    assert.deepEqual(state.entities.get(id)?.properties, row.properties);
  }
  assert.ok(chunks > 1); assert.equal(snapshots.size, 1); assert.equal(joinStateCount, 0);
  assert.ok(maxFrame <= 65536); assert.ok(maxNormalFrame <= 16384);
  console.log(JSON.stringify({ pass: true, instanceId, entities: expected.size, allCustomMatchPG: true, chunks, maxFrame, maxNormalFrame, negotiatedJoinStateCount: joinStateCount }));
} finally { await connection.disconnect(); }
