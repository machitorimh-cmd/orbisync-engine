#!/usr/bin/env node
// External lock authority for the whiteboard experiment.
// The Core component is deliberately not used as the lock source of truth.
import http from "node:http";
import https from "node:https";
import crypto from "node:crypto";
import fs from "node:fs";
import { pathToFileURL } from "node:url";

const CORE_URL = process.env.ORBISYNC_CORE_URL || "http://127.0.0.1:18081";
const API_PORT = Number(process.env.WHITEBOARD_LOCK_API_PORT || 8877);
const HOOK_PORT = Number(process.env.WHITEBOARD_LOCK_HOOK_PORT || 8443);
const ALLOWED_ORIGIN = process.env.WHITEBOARD_LOCK_ALLOWED_ORIGIN || "http://127.0.0.1:5175";
const SECRET_ENV = process.env.WHITEBOARD_LOCK_SECRET_ENV || "ORBI_EXTENSION_SECRET_WHITEBOARD_LOCK";
const STORE_PATH = process.env.WHITEBOARD_LOCK_STORE || "./whiteboard-locks.json";
const TTL_MS = 30_000;
const locks = new Map();

function readStore() { try { const entries = JSON.parse(fs.readFileSync(STORE_PATH, "utf8")); for (const [key, value] of Object.entries(entries)) locks.set(key, value); } catch { /* first run */ } }
function writeStore() { const out = Object.fromEntries(locks); const tmp = `${STORE_PATH}.tmp`; fs.writeFileSync(tmp, JSON.stringify(out, null, 2)); fs.renameSync(tmp, STORE_PATH); }
function key(instanceId, entityId) { return `${instanceId}/${entityId}`; }
function prune() { const now = Date.now(); for (const [k, lock] of locks) if (lock.status !== "locked" && lock.expires_at <= now) locks.delete(k); }
if (ALLOWED_ORIGIN === "*") throw new Error("WHITEBOARD_LOCK_ALLOWED_ORIGIN must be an explicit origin");
function json(res, status, body) { res.writeHead(status, { "content-type": "application/json", "access-control-allow-origin": ALLOWED_ORIGIN, "access-control-allow-headers": "authorization,content-type", "access-control-allow-methods": "GET,POST,OPTIONS", vary: "Origin" }); res.end(JSON.stringify(body)); }
function body(req) { return new Promise((resolve, reject) => { let text = ""; req.on("data", chunk => { text += chunk; if (text.length > 64 * 1024) reject(new Error("body too large")); }); req.on("end", () => { try { resolve(text ? JSON.parse(text) : {}); } catch { reject(new Error("invalid JSON")); } }); req.on("error", reject); }); }
async function authenticate(req) {
  const authorization = req.headers.authorization;
  if (!authorization?.startsWith("Bearer ")) return null;
  try {
    const response = await fetch(`${CORE_URL}/v1/auth/me`, { headers: { authorization } });
    if (!response.ok) return null;
    const user = await response.json();
    return typeof user.id === "string" ? user : null;
  } catch {
    return { unavailable: true };
  }
}
function validIds(value) { return typeof value === "string" && /^[0-9a-f-]{36}$/i.test(value); }
function tokenEquals(a, b) { const aa = Buffer.from(a || ""); const bb = Buffer.from(b || ""); return aa.length === bb.length && aa.length > 0 && crypto.timingSafeEqual(aa, bb); }
function verifyHook(raw, headers) { const secret = process.env[SECRET_ENV]; const sig = headers["x-orbisync-signature"]; const eventId = headers["x-orbisync-event-id"]; const stamp = Number(headers["x-orbisync-timestamp"]); if (!secret || !sig || !eventId || !Number.isFinite(stamp) || Math.abs(Math.floor(Date.now() / 1000) - stamp) > 300) return false; const expected = `sha256=${crypto.createHmac("sha256", secret).update(`${stamp}.${eventId}.${raw}`).digest("hex")}`; return tokenEquals(sig, expected); }
function requestPayload(envelope) { return envelope?.payload || {}; }
function decide(envelope) { prune(); const p = requestPayload(envelope); const args = p.payload || {}; const instanceId = p.instance_id; const entityId = p.entity_id; const record = locks.get(key(instanceId, entityId)); if (!record) return { decision: "allow" };
  const requester = p.requester; const revision = p.client_expected_revision == null ? null : Number(p.client_expected_revision); const lockId = typeof args.whiteboard_lock_id === "string" ? args.whiteboard_lock_id : "";
  if (record.status === "locking" && p.operation === "precommit.entity.update" && args.locked === true && record.owner_id === requester && tokenEquals(lockId, record.lock_id) && (record.expected_revision == null || record.expected_revision === revision)) return { decision: "allow" };
  if (record.status === "unlocking" && p.operation === "precommit.entity.update" && args.locked === false && record.owner_id === requester && tokenEquals(lockId, record.lock_id) && record.expected_revision === revision) return { decision: "allow" };
  return { decision: "deny", reason: "付箋は外部ロック中です。所有者の専用解除操作が必要です。" };
}
async function handleApi(req, res) {
  try {
    if (req.method === "OPTIONS") return json(res, 204, {});
    const user = await authenticate(req);
    if (user?.unavailable) return json(res, 503, { error: "AUTHENTICATION_UNAVAILABLE" });
    if (!user) return json(res, 401, { error: "AUTHENTICATION_REQUIRED" });
    prune();
    const url = new URL(req.url, `http://${req.headers.host}`);
    if (req.method === "GET" && url.pathname === "/v1/whiteboard/locks") {
      const instanceId = url.searchParams.get("instance_id");
      const result = [...locks.values()].filter(lock => !instanceId || lock.instance_id === instanceId).map(lock => ({ instance_id: lock.instance_id, entity_id: lock.entity_id, owner_id: lock.owner_id, status: lock.status, lock_id: lock.lock_id }));
      return json(res, 200, { locks: result });
    }
    if (req.method !== "POST" || !["/v1/whiteboard/locks/lock", "/v1/whiteboard/locks/unlock", "/v1/whiteboard/locks/finalize", "/v1/whiteboard/locks/cancel"].includes(url.pathname)) return json(res, 404, { error: "RESOURCE_NOT_FOUND" });
    let input;
    try { input = await body(req); } catch { return json(res, 400, { error: "INVALID_REQUEST" }); }
    try {
      if (!validIds(input.instance_id) || !validIds(input.entity_id)) return json(res, 400, { error: "INVALID_REQUEST" });
      const k = key(input.instance_id, input.entity_id); const current = locks.get(k);
      if (url.pathname.endsWith("/lock")) { if (current) return json(res, 409, { error: "LOCKED" }); const lock = { instance_id: input.instance_id, entity_id: input.entity_id, owner_id: user.id, expected_revision: input.expected_revision == null ? null : Number(input.expected_revision), lock_id: crypto.randomUUID(), status: "locking", expires_at: Date.now() + TTL_MS }; locks.set(k, lock); writeStore(); return json(res, 201, { lock }); }
      if (!current || current.owner_id !== user.id) return json(res, 403, { error: "LOCK_OWNER_REQUIRED" }); if (url.pathname.endsWith("/unlock")) { if (!Number.isSafeInteger(input.expected_revision) || input.expected_revision < 1) return json(res, 400, { error: "INVALID_REVISION" }); current.expected_revision = input.expected_revision; current.status = "unlocking"; current.expires_at = Date.now() + TTL_MS; writeStore(); return json(res, 200, { lock: current }); } if (!tokenEquals(input.lock_id, current.lock_id)) return json(res, 409, { error: "LOCK_TOKEN_MISMATCH" }); if (url.pathname.endsWith("/finalize")) { const validFinalize = (input.action === "unlock" && current.status === "unlocking") || (input.action === "lock" && current.status === "locking"); if (!validFinalize) return json(res, 409, { error: "LOCK_STATE_MISMATCH" }); if (input.action === "unlock") locks.delete(k); else { current.status = "locked"; current.expires_at = Date.now() + TTL_MS; } writeStore(); return json(res, 200, { ok: true }); } if (url.pathname.endsWith("/cancel")) { locks.delete(k); writeStore(); return json(res, 200, { ok: true }); }
    } catch { return json(res, 500, { error: "LOCK_STORE_UNAVAILABLE" }); }
  } catch { return json(res, 500, { error: "LOCK_SERVICE_ERROR" }); }
}
function handleHook(req, res) { if (req.method !== "POST") return res.writeHead(405).end(); let raw = ""; req.on("data", chunk => { raw += chunk; if (raw.length > 64 * 1024) req.destroy(); }); req.on("end", () => { if (!verifyHook(raw, req.headers)) return json(res, 401, { decision: "deny", reason: "signature rejected" }); try { const result = decide(JSON.parse(raw)); console.log(`whiteboard.precommit -> ${result.decision}${result.reason ? ` (${result.reason})` : ""}`); json(res, 200, result); } catch { json(res, 400, { decision: "deny", reason: "invalid request" }); } }); }
function main() { readStore(); const api = http.createServer((req, res) => void handleApi(req, res)); api.listen(API_PORT, "127.0.0.1", () => console.log(`whiteboard lock API listening on http://127.0.0.1:${API_PORT}`)); const cert = process.env.WHITEBOARD_LOCK_CERT || "cert.pem"; const keyFile = process.env.WHITEBOARD_LOCK_KEY || "key.pem"; if (process.env.WHITEBOARD_LOCK_SKIP_HOOK === "1") return; if (!fs.existsSync(cert) || !fs.existsSync(keyFile)) throw new Error(`TLS files missing: ${cert}, ${keyFile}`); const hook = https.createServer({ cert: fs.readFileSync(cert), key: fs.readFileSync(keyFile) }, handleHook); hook.listen(HOOK_PORT, "127.0.0.1", () => console.log(`whiteboard precommit hook listening on https://127.0.0.1:${HOOK_PORT}/precommit`)); }
if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) main();
