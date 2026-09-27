/** Opt-in extension API smoke test against a disposable real server and DB.
 * Requires live-runtime.ts exercise to have created live-state.json first.
 * Operator CLI and HTTP are exercised together; credentials stay outside the repo.
 */
import assert from "node:assert/strict";
import { readFileSync, writeFileSync, rmSync } from "node:fs";
import { spawnSync } from "node:child_process";
import path from "node:path";
import { WebSocket } from "ws";
import { OrbiSyncClient, createUuidV7 } from "../src/client.js";

Object.assign(globalThis, { WebSocket });
const dir = process.env.ORBISYNC_LIVE_TEST_DIR;
const baseUrl = process.env.ORBISYNC_LIVE_TEST_URL;
assert.ok(dir && baseUrl, "explicit disposable test environment required");
const state = JSON.parse(readFileSync(path.join(dir, "live-state.json"), "utf8"));
const env = { ...process.env, ...JSON.parse(readFileSync(path.join(dir, "env.json"), "utf8")) };
const extensionId = createUuidV7();
const manifestFile = path.join(dir, `extension-${extensionId}.json`);
const firstFile = path.join(dir, `extension-${extensionId}-1.token`);
const secondFile = path.join(dir, `extension-${extensionId}-2.token`);
const manifest = {
  extension_id: extensionId, name: "functional extension", description: null,
  endpoint: "https://example.invalid/webhook", subscribed_events: [],
  capabilities: ["commands:entity:read", "commands:audit:read"],
  token_scopes: ["commands:entity:read", "commands:audit:read", `instances:${state.instanceId}`],
  status: "active", signing_secret_ref: "EXTENSION_TEST_SIGNING_KEY",
};
const binary = path.resolve("target/debug/orbisync-server.exe");
function cli(...args: string[]) {
  const result = spawnSync(binary, ["--config", path.join(dir!, "config.toml"), ...args], { env, encoding: "utf8", windowsHide: true });
  assert.equal(result.status, 0, `operator command failed: ${args[0]}`);
  return result.stdout + result.stderr;
}
async function request(token: string, body: unknown, expected: number, endpoint = "/v1/extensions/commands") {
  const response = await fetch(baseUrl + endpoint, { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type": "application/json" }, body: JSON.stringify(body) });
  assert.equal(response.status, expected, endpoint);
  const result = await response.json();
  assert.ok(!JSON.stringify(result).includes(token));
  return result;
}
const client = new OrbiSyncClient({ baseUrl });
await client.auth.login({ loginId: "admin", password: readFileSync(path.join(dir, "admin-password.txt"), "utf8").trim() });
const connection = await client.connect();
try {
  const instance = await connection.join(state.instanceId);
  await new Promise<void>((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("initial snapshot timeout")), 6000);
    instance.on("snapshotApplied", () => { clearTimeout(timer); resolve(); });
  });
  writeFileSync(manifestFile, JSON.stringify(manifest));
  let logs = cli("extension-register", "--manifest", manifestFile);
  const issue = (output: string) => cli("extension-token", "--extension-id", extensionId,
    ...manifest.token_scopes.flatMap(scope => ["--scope", scope]), "--token-output", output);
  logs += issue(firstFile);
  const first = readFileSync(firstFile, "utf8").trim();
  const command = { command: "entity.get", instance_id: state.instanceId, entity_id: state.entityId };
  const read = await request(first, command, 200);
  assert.equal(read.result.entity_id, state.entityId);
  assert.equal(JSON.parse(Buffer.from(read.result.components["test.state"]).toString()).value, 42);
  await request(first, { ...command, instance_id: state.otherInstanceId }, 403);
  await request(first, { login_id: "forbidden", display_name: "forbidden" }, 401, "/v1/users");
  logs += issue(secondFile);
  const second = readFileSync(secondFile, "utf8").trim();
  await request(first, command, 401);
  await request(second, command, 200);
  const audit = await (await client.fetch("/v1/audit-events?limit=50")).json();
  const entry = audit.items.find((item: { action: string; resource_id?: string }) => item.action === "extension.token_rotated" && item.resource_id === extensionId);
  assert.ok(entry, "rotation audit must be visible through the public audit API");
  const auditResult = await request(second, { command: "audit.get", event_id: entry.audit_id }, 200);
  assert.equal(auditResult.result.action, "extension.token_rotated");
  manifest.status = "suspended"; writeFileSync(manifestFile, JSON.stringify(manifest));
  logs += cli("extension-register", "--manifest", manifestFile);
  await request(second, command, 401);
  manifest.status = "active"; writeFileSync(manifestFile, JSON.stringify(manifest));
  logs += cli("extension-register", "--manifest", manifestFile);
  await request(second, command, 200);
  logs += cli("extension-revoke", "--extension-id", extensionId);
  await request(second, command, 401);
  logs += readFileSync(path.join(dir, "server.log"), "utf8");
  for (const token of [first, second]) assert.ok(!logs.includes(token), "credential must not enter operator/server logs");
  console.log("PASS: real CLI register/issue/rotate/revoke, scoped entity/component and audit reads, cross-instance/admin rejection, suspension, and credential-free logs");
} finally {
  await connection.disconnect();
  for (const file of [manifestFile, firstFile, secondFile]) rmSync(file, { force: true });
}
