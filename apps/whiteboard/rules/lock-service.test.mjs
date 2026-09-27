import test from "node:test";
import assert from "node:assert/strict";
import http from "node:http";
import os from "node:os";
import path from "node:path";
import { randomUUID } from "node:crypto";
import { spawn } from "node:child_process";

function listen(server) {
  return new Promise((resolve) => server.listen(0, "127.0.0.1", () => resolve(server.address().port)));
}

function waitForOutput(child, text) {
  return new Promise((resolve, reject) => {
    let output = "";
    const onData = (chunk) => {
      output += chunk;
      if (output.includes(text)) { child.stdout.off("data", onData); resolve(); }
    };
    child.stdout.on("data", onData);
    child.once("exit", (code) => reject(new Error(`lock service exited before readiness: ${code}`)));
  });
}

async function request(port, headers = {}) {
  return fetch(`http://127.0.0.1:${port}/v1/whiteboard/locks`, { headers });
}

test("Core fetch rejection returns structured 503 and the service remains alive", async (t) => {
  const reservation = http.createServer();
  const apiPort = await listen(reservation);
  // Close the reservation before starting the lock service on this port.
  await new Promise((resolve) => reservation.close(resolve));
  const store = path.join(os.tmpdir(), `orbisync-lock-test-${randomUUID()}.json`);
  const child = spawn(process.execPath, [path.join(process.cwd(), "rules", "lock-service.mjs")], {
    cwd: process.cwd(),
    env: { ...process.env, ORBISYNC_CORE_URL: "http://127.0.0.1:1", WHITEBOARD_LOCK_API_PORT: String(apiPort), WHITEBOARD_LOCK_SKIP_HOOK: "1", WHITEBOARD_LOCK_STORE: store },
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(async () => {
    child.kill();
    try { await import("node:fs/promises").then((fs) => fs.unlink(store)); } catch {}
  });
  await waitForOutput(child, "lock API listening");
  const unavailable = await request(apiPort, { authorization: "Bearer test-token" });
  assert.equal(unavailable.status, 503);
  assert.deepEqual(await unavailable.json(), { error: "AUTHENTICATION_UNAVAILABLE" });
  assert.equal(child.exitCode, null);

  const unauthorized = await request(apiPort);
  assert.equal(unauthorized.status, 401);
  assert.deepEqual(await unauthorized.json(), { error: "AUTHENTICATION_REQUIRED" });
});

test("successful authentication still serves the lock list", async (t) => {
  const core = http.createServer((_req, res) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ id: "user-1" })); });
  const corePort = await listen(core);
  const reservation = http.createServer();
  const apiPort = await listen(reservation);
  await new Promise((resolve) => reservation.close(resolve));
  const store = path.join(os.tmpdir(), `orbisync-lock-test-${randomUUID()}.json`);
  const child = spawn(process.execPath, [path.join(process.cwd(), "rules", "lock-service.mjs")], {
    cwd: process.cwd(),
    env: { ...process.env, ORBISYNC_CORE_URL: `http://127.0.0.1:${corePort}`, WHITEBOARD_LOCK_API_PORT: String(apiPort), WHITEBOARD_LOCK_SKIP_HOOK: "1", WHITEBOARD_LOCK_STORE: store },
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(async () => { child.kill(); core.close(); try { await import("node:fs/promises").then((fs) => fs.unlink(store)); } catch {} });
  await waitForOutput(child, "lock API listening");
  const response = await request(apiPort, { authorization: "Bearer test-token" });
  assert.equal(response.status, 200);
  assert.deepEqual(await response.json(), { locks: [] });
});

test("owner unlock binds to the current revision, not the pre-lock revision", async (t) => {
  const core = http.createServer((_req, res) => { res.writeHead(200, { "content-type": "application/json" }); res.end(JSON.stringify({ id: "owner" })); });
  const corePort = await listen(core);
  const reservation = http.createServer();
  const apiPort = await listen(reservation);
  await new Promise(resolve => reservation.close(resolve));
  const store = path.join(os.tmpdir(), `orbisync-lock-test-${randomUUID()}.json`);
  const child = spawn(process.execPath, [path.join(process.cwd(), "rules", "lock-service.mjs")], {
    env: { ...process.env, ORBISYNC_CORE_URL: `http://127.0.0.1:${corePort}`, WHITEBOARD_LOCK_API_PORT: String(apiPort), WHITEBOARD_LOCK_SKIP_HOOK: "1", WHITEBOARD_LOCK_STORE: store },
    stdio: ["ignore", "pipe", "pipe"],
  });
  t.after(async () => { child.kill(); core.close(); try { await import("node:fs/promises").then(fs => fs.unlink(store)); } catch {} });
  await waitForOutput(child, "lock API listening");
  const ids = { instance_id: randomUUID(), entity_id: randomUUID() };
  const post = (action, extra) => fetch(`http://127.0.0.1:${apiPort}/v1/whiteboard/locks/${action}`, {
    method: "POST", headers: { authorization: "Bearer test-token", "content-type": "application/json" }, body: JSON.stringify({ ...ids, ...extra }),
  });
  const created = await post("lock", { expected_revision: 1 });
  assert.equal(created.status, 201);
  const { lock } = await created.json();
  assert.equal((await post("finalize", { lock_id: lock.lock_id, action: "lock" })).status, 200);
  assert.equal((await post("unlock", {})).status, 400);
  const response = await post("unlock", { expected_revision: 2 });
  assert.equal(response.status, 200);
  const { lock: unlocking } = await response.json();
  assert.equal(unlocking.status, "unlocking");
  assert.equal(unlocking.expected_revision, 2);
  assert.equal(unlocking.owner_id, "owner");
  assert.equal(unlocking.lock_id, lock.lock_id);
  const stored = JSON.parse(await import("node:fs/promises").then(fs => fs.readFile(store, "utf8")));
  assert.equal(stored[`${ids.instance_id}/${ids.entity_id}`].expected_revision, 2);
  assert.equal((await post("finalize", { lock_id: lock.lock_id, action: "unlock" })).status, 200);
  assert.deepEqual(await (await request(apiPort, { authorization: "Bearer test-token" })).json(), { locks: [] });
});
