/** Opt-in functional smoke test against a real, disposable server and database.
 * Run with ORBISYNC_LIVE_TEST_DIR (outside the repository, containing the
 * bootstrap admin-password.txt) and ORBISYNC_LIVE_TEST_URL. Run exercise,
 * then shutdown, restart the same server/database, and finally run restore.
 * The shutdown phase writes shutdown-ready after an acknowledged update,
 * then waits for the server's shutdown notification (send Ctrl+C externally).
 * Uses three clients and a few entities; this is not a load test.
 */
import assert from "node:assert/strict";
import { readFileSync, rmSync, writeFileSync } from "node:fs";
import path from "node:path";
import { WebSocket } from "ws";
import { OrbiSyncClient, createUuidV7, type OrbiSyncInstance, type OrbiSyncConnection } from "../src/client.js";

Object.assign(globalThis, { WebSocket });
const dir = process.env.ORBISYNC_LIVE_TEST_DIR;
const baseUrl = process.env.ORBISYNC_LIVE_TEST_URL;
assert.ok(dir && baseUrl, "explicit disposable live-test directory and URL required");
const phase = process.argv[2];
assert.ok(phase === "exercise" || phase === "restore" || phase === "shutdown");
if (phase === "shutdown") rmSync(path.join(dir, "shutdown-ready"), { force: true });
const connections: OrbiSyncConnection[] = [];
const sleep = (ms: number) => new Promise<void>(resolve => setTimeout(resolve, ms));

async function api(client: OrbiSyncClient, method: string, url: string, body?: unknown, status = 200, headers: Record<string, string> = {}) {
  const response = await client.fetch(url, {
    method, headers: { "content-type": "application/json", ...headers },
    body: body === undefined ? undefined : JSON.stringify(body),
  });
  assert.equal(response.status, status, `${method} ${url}`);
  const text = await response.text();
  return text ? JSON.parse(text) : null;
}

function event(instance: OrbiSyncInstance, name: string, predicate: (value: any) => boolean = () => true, timeout = 6000): Promise<any> {
  return new Promise((resolve, reject) => {
    const handler = (value: any) => { if (predicate(value)) { clearTimeout(timer); instance.off(name, handler); resolve(value); } };
    const timer = setTimeout(() => { instance.off(name, handler); reject(new Error(`Timed out: ${name}`)); }, timeout);
    instance.on(name, handler);
  });
}

async function join(client: OrbiSyncClient, instanceId: string) {
  const connection = await client.connect(); connections.push(connection);
  assert.equal(connection.getConnectionState().sessionState, "Ready");
  const instance = await connection.join(instanceId);
  const snapshot = await event(instance, "snapshot");
  assert.equal(snapshot.chunkCount, 1, "small test snapshot must fit one chunk");
  assert.equal(connection.getConnectionState().sessionState, "Active");
  assert.equal(connection.getConnectionState().lastAppliedRevision, snapshot.instanceRevision);
  const data = JSON.parse(Buffer.from(snapshot.data).toString());
  assert.equal(data.instance.instance_id, instanceId);
  return { connection, instance, data };
}

async function command(instance: OrbiSyncInstance, entityId: string, operation: string, args: Record<string, unknown> = {}, expectedRevision?: bigint) {
  const result = event(instance, "entityUpdated", v => v.entityId === entityId);
  instance.sendEntityCommand({ entityId, operation, args, expectedRevision });
  return result;
}

try {
  for (const endpoint of ["/health/live", "/health/ready"]) {
    assert.equal((await fetch(baseUrl + endpoint)).status, 200, endpoint);
  }
  const admin = new OrbiSyncClient({ baseUrl });
  await admin.auth.login({ loginId: "admin", password: readFileSync(path.join(dir, "admin-password.txt"), "utf8").trim() });
  assert.ok((await api(admin, "GET", "/v1/auth/me")).id);
  if (phase === "shutdown") {
    const state = JSON.parse(readFileSync(path.join(dir, "live-state.json"), "utf8"));
    const { instance } = await join(admin, state.instanceId);
    await command(instance, state.entityId, "update", { component_key: "test.state", value: 44 });
    const notification = event(instance, "error", v => v.code === "SERVER_SHUTTING_DOWN", 30000);
    writeFileSync(path.join(dir, "shutdown-ready"), "ready");
    await notification;
    console.log("PASS: connected client receives SERVER_SHUTTING_DOWN after accepted component update");
  } else if (phase === "restore") {
    const state = JSON.parse(readFileSync(path.join(dir, "live-state.json"), "utf8"));
    const { instance, data } = await join(admin, state.instanceId);
    const restored = data.entities.find((e: any) => e.entity_id === state.entityId);
    assert.ok(restored, "persistent entity restored through real startup and join");
    assert.ok(restored.revision >= state.revision);
    assert.equal(restored.transform.position.x, 2);
    assert.ok(restored.components["test.state"], "restored custom state must reach the client");
    const savedComponent = JSON.parse(Buffer.from(restored.components["test.state"]).toString("utf8"));
    assert.equal(savedComponent.value, 44);
    assert.ok(!data.entities.some((e: any) => e.entity_id === state.deletedId), "deleted entity must not reappear");
    const result = await command(instance, state.entityId, "update", { component_key: "test.state", value: 43 });
    assert.ok(Number(result.revision ?? result.expectedRevision) > state.revision, "restored state remains writable");
    console.log("PASS: restart restores entity, transform, revision and deletion; component remains writable");
  } else {
    const world = await api(admin, "POST", "/v1/worlds", { name: "Functional live test", capacity: 5 }, 201);
    const created = await api(admin, "POST", "/v1/instances", { world_id: world.id, capacity: 5 }, 201);
    const instanceId = created.id;
    const role = await api(admin, "POST", "/v1/roles", { name: `Functional player ${Date.now()}`, permissions: ["entity.spawn", "entity.update.own"] }, 201);
    const suffix = Date.now().toString(36);
    const user = await api(admin, "POST", "/v1/users", { login_id: `player-${suffix}`, display_name: "Functional player" }, 201, { accept: "application/vnd.orbisync.user-credential+json" });
    const userId = user.id ?? user.user?.id;
    assert.ok(userId && user.temporary_password);
    await api(admin, "PUT", `/v1/users/${userId}/roles`, { role_ids: [role.id] });
    const player = new OrbiSyncClient({ baseUrl });
    await player.auth.login({ loginId: `player-${suffix}`, password: user.temporary_password });
    await api(player, "POST", "/v1/worlds", { name: "Forbidden", capacity: 5 }, 403);
    const a = await join(admin, instanceId);
    const b = await join(player, instanceId);
    assert.equal(b.instance.getJoinInfo().permissions.entityUpdateAny, false);
    console.log("PASS: real auth, user/role/world/instance APIs, denied administration, tickets, protobuf join and initial snapshot");

    const entityId = createUuidV7();
    const spawn = { kind: "object", visibility: "global", position_x: 1, position_y: 0, position_z: 0 };
    let seen = event(b.instance, "entityUpdated", v => v.entityId === entityId);
    await command(a.instance, entityId, "spawn", spawn); await seen;
    const denied = event(b.instance, "error");
    b.instance.sendEntityCommand({ entityId, operation: "update", args: { component_key: "test.state", value: 99 } });
    assert.equal((await denied).code, "NOT_OWNER");
    await sleep(300);
    seen = event(b.instance, "entityUpdated", v => v.entityId === entityId && v.transform?.positionX === 2);
    a.instance.sendTransform({ entityId, position: { x: 2, y: 0, z: 0 } }); await seen;
    seen = event(b.instance, "entityUpdated", v => v.entityId === entityId);
    const updated = await command(a.instance, entityId, "update", { component_key: "test.state", value: 42 }); await seen;
    const revision = Number(updated.revision ?? updated.expectedRevision);
    console.log("PASS: spawn, transform and component delivery; non-owner write rejected");

    const secretId = createUuidV7();
    let leaked = false;
    const observe = (v: any) => { if (v.entityId === secretId) leaked = true; };
    b.instance.on("entityUpdated", observe);
    await command(a.instance, secretId, "spawn", { ...spawn, visibility: "owner_only" });
    await command(a.instance, secretId, "update", { component_key: "test.secret", value: 7 });
    await sleep(300); assert.equal(leaked, false, "owner-only data must not leak");
    b.instance.off("entityUpdated", observe);
    const transferId = createUuidV7();
    await command(a.instance, transferId, "spawn", { ...spawn, visibility: "owner_only" });
    seen = event(b.instance, "entityUpdated", v => v.entityId === transferId);
    a.instance.transferEntityOwnership({ entityId: transferId, newOwnerId: userId });
    await seen;
    let formerOwnerSawUpdate = false;
    a.instance.on("entityUpdated", (v: any) => { if (v.entityId === transferId && v.operation === "update") formerOwnerSawUpdate = true; });
    await command(b.instance, transferId, "update", { component_key: "test.secret", value: 8 });
    await sleep(300);
    assert.equal(formerOwnerSawUpdate, false, "ownership change must immediately change visibility");
    console.log("PASS: ownership transfer updates write permission and owner-only delivery immediately");
    const deletedId = createUuidV7();
    await command(a.instance, deletedId, "spawn", spawn);
    seen = event(b.instance, "entityUpdated", v => v.entityId === deletedId && v.operation === "delete");
    await command(a.instance, deletedId, "delete"); await seen;

    const oldToken = b.connection._getResumeToken();
    const resumed = event(b.instance, "resumeAccepted", () => true, 12000);
    b.connection._forceCloseTransport(4000, "functional resume check");
    await resumed;
    assert.notEqual(b.connection._getResumeToken(), oldToken);
    seen = event(b.instance, "entityUpdated", v => v.entityId === entityId);
    await command(a.instance, entityId, "update", { component_key: "test.state", value: 42 }); await seen;
    await b.connection.disconnect();
    const fresh = await join(player, instanceId);
    assert.ok(fresh.data.entities.some((e: any) => e.entity_id === entityId));
    const freshEntity = fresh.data.entities.find((e: any) => e.entity_id === entityId);
    assert.equal(JSON.parse(Buffer.from(freshEntity.components["test.state"]).toString("utf8")).value, 42);
    assert.ok(!fresh.data.entities.some((e: any) => e.entity_id === secretId || e.entity_id === deletedId));
    console.log("PASS: owner-only filtering, reliable deletion, resume token rotation, continued delivery, leave and fresh snapshot");

    const world2 = await api(admin, "POST", "/v1/worlds", { name: "Functional isolation", capacity: 5 }, 201);
    const other = await api(admin, "POST", "/v1/instances", { world_id: world2.id }, 201);
    const otherClient = new OrbiSyncClient({ baseUrl });
    await otherClient.auth.login({ loginId: "admin", password: readFileSync(path.join(dir, "admin-password.txt"), "utf8").trim() });
    const c = await join(otherClient, other.id);
    assert.ok(!c.data.entities.some((e: any) => e.entity_id === entityId));
    const otherId = createUuidV7();
    let crossed = false;
    a.instance.on("entityUpdated", (v: any) => { if (v.entityId === otherId) crossed = true; });
    await command(c.instance, otherId, "spawn", spawn);
    await sleep(300); assert.equal(crossed, false);
    let crossRoomMessage = false;
    c.instance.on("domainEvent", () => { crossRoomMessage = true; });
    const echoed = event(a.instance, "domainEvent");
    const delivered = event(fresh.instance, "domainEvent");
    a.instance.sendDomainEvent({ eventType: "custom.chat.message", data: { text: "こんにちは", sender_user_id: "forged" } });
    const [echo, message] = await Promise.all([echoed, delivered]);
    assert.equal(message.eventId, echo.eventId);
    assert.equal(message.data.text, "こんにちは");
    assert.equal(message.data.sender_user_id, a.instance.getJoinInfo().userId);
    await sleep(300); assert.equal(crossRoomMessage, false);
    console.log("PASS: custom message delivery, authoritative sender, room isolation, and complete component snapshot");
    const ensureToken = player.ensureValidAccessToken.bind(player);
    let releaseReconnect!: () => void;
    const reconnectGate = new Promise<void>(resolve => { releaseReconnect = resolve; });
    player.ensureValidAccessToken = async () => { await reconnectGate; await ensureToken(); };
    const replay = event(fresh.instance, "domainEvent", v => v.data?.text === "切断中のメッセージ");
    const restored = event(fresh.instance, "snapshotApplied");
    const restoredWire = event(fresh.instance, "snapshot");
    fresh.connection.requestReconnect();
    await sleep(150);
    const offlineEcho = event(a.instance, "domainEvent", v => v.data?.text === "切断中のメッセージ");
    a.instance.sendDomainEvent({ eventType: "custom.chat.message", data: { text: "切断中のメッセージ" } });
    const acceptedOffline = await offlineEcho;
    releaseReconnect();
    assert.equal((await replay).eventId, acceptedOffline.eventId);
    await restored;
    const restoredSnapshot = await restoredWire;
    // The reconnect continuation publishes Active after applying the snapshot.
    await sleep(0);
    assert.equal(fresh.connection.getConnectionState().sessionState, "Active");
    assert.equal(fresh.connection.getConnectionState().lastAppliedRevision, restoredSnapshot.instanceRevision);
    player.ensureValidAccessToken = ensureToken;
    assert.equal(crossRoomMessage, false);
    console.log("PASS: real authenticated resume replays the offline message before snapshot application");
    writeFileSync(path.join(dir, "live-state.json"), JSON.stringify({ instanceId, entityId, deletedId, secretId, otherInstanceId: other.id, otherId, revision }));
    console.log("PASS: two independent instances persist and deliver without cross-world leakage");
  }
} finally {
  await Promise.all(connections.map(c => c.disconnect()));
}
